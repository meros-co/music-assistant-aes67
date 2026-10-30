// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! NMOS: an IS-04 v1.3 node with one device, a source, flow and sender per
//! stream, and the IS-05 v1.1 connection API for those senders.
//!
//! The node registers with a registry found through mDNS
//! (`_nmos-register._tcp`) and heartbeats every 5 s; with no registry it
//! advertises itself (`_nmos-node._tcp`) for peer-to-peer discovery. IS-05
//! changes (enable, destination) apply at once; whoever configured the
//! outputs (the add-on options, or Music Assistant) stays the stored truth.

use std::collections::{BTreeMap, HashMap};
use std::net::Ipv4Addr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tiny_http::{Header, Method, Response, Server};
use uuid::Uuid;

use crate::engine::Engine;
use crate::log;
use crate::output::OutputConfig;
use crate::stream::StreamKey;

const NS: Uuid = Uuid::from_u128(0x6f2c_9b1e_7d4a_4c0b_9a52_3e1f_0d8c_aa67);

pub struct NmosConfig {
    pub label: String,
    pub hostname: String,
    pub port: u16,
    pub interface: String,
    pub mac: Option<String>,
    /// Stable identity for every id this node hands out.
    pub seed: String,
}

pub struct Nmos {
    engine: Arc<Engine>,
    cfg: NmosConfig,
    ip: Ipv4Addr,
    /// version strings per resource id: they change only when the resource does.
    versions: Mutex<HashMap<Uuid, (String, String)>>,
    staged: Mutex<HashMap<Uuid, Value>>,
}

fn tai_version() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}:{}", d.as_secs() + 37, d.subsec_nanos())
}

impl Nmos {
    pub fn new(engine: Arc<Engine>, cfg: NmosConfig) -> Arc<Self> {
        let ip = engine.net.iface_ip;
        Arc::new(Nmos {
            engine,
            cfg,
            ip,
            versions: Mutex::new(HashMap::new()),
            staged: Mutex::new(HashMap::new()),
        })
    }

    pub fn id(&self, what: &str) -> Uuid {
        Uuid::new_v5(&NS, format!("{}/{}", self.cfg.seed, what).as_bytes())
    }

    fn node_id(&self) -> Uuid {
        self.id("node")
    }

    fn device_id(&self) -> Uuid {
        self.id("device")
    }

    /// A resource's version, kept while its content is unchanged.
    fn version(&self, id: Uuid, body: &Value) -> String {
        let text = body.to_string();
        let mut v = self.versions.lock().unwrap();
        match v.get(&id) {
            Some((t, ver)) if *t == text => ver.clone(),
            _ => {
                let ver = tai_version();
                v.insert(id, (text, ver.clone()));
                ver
            }
        }
    }

    fn with_version(&self, id: Uuid, mut body: Value) -> Value {
        let ver = self.version(id, &body);
        body["version"] = json!(ver);
        body
    }

    fn base(&self) -> String {
        format!("http://{}:{}", self.ip, self.cfg.port)
    }

    pub fn node(&self) -> Value {
        let status = self.engine.clock_status();
        let clock = match &status.grandmaster {
            Some(gm) => {
                json!({"name": "clk0", "ref_type": "ptp", "traceable": false, "version": "IEEE1588-2008", "gmid": gm.to_lowercase(), "locked": status.locked})
            }
            None => json!({"name": "clk0", "ref_type": "internal"}),
        };
        let id = self.node_id();
        self.with_version(
            id,
            json!({
                "id": id,
                "label": self.cfg.label,
                "description": "AES67 sender for Music Assistant",
                "tags": {},
                "href": format!("{}/", self.base()),
                "hostname": self.cfg.hostname,
                "api": {"versions": ["v1.3"], "endpoints": [{"host": self.ip.to_string(), "port": self.cfg.port, "protocol": "http", "authorization": false}]},
                "caps": {},
                "services": [],
                "clocks": [clock],
                "interfaces": [{"name": self.cfg.interface, "chassis_id": null, "port_id": self.cfg.mac.clone().unwrap_or_else(|| "00-00-00-00-00-00".into())}],
            }),
        )
    }

    /// A stream's resources are named after its first output, so they keep
    /// their ids when IS-05 moves the stream to a new destination.
    fn stream_uid(&self, what: &str, ids: &[String]) -> Uuid {
        self.id(&format!(
            "{what}/{}",
            ids.first().map(String::as_str).unwrap_or("")
        ))
    }

    pub fn device(&self) -> Value {
        let senders: Vec<Uuid> = self
            .engine
            .streams()
            .iter()
            .map(|s| self.stream_uid("sender", &s.2))
            .collect();
        let id = self.device_id();
        self.with_version(
            id,
            json!({
                "id": id,
                "label": self.cfg.label,
                "description": "AES67 outputs",
                "tags": {},
                "type": "urn:x-nmos:device:generic",
                "node_id": self.node_id(),
                "senders": senders,
                "receivers": [],
                "controls": [{"type": "urn:x-nmos:control:sr-ctrl/v1.1", "href": format!("{}/x-nmos/connection/v1.1/", self.base()), "authorization": false}],
            }),
        )
    }

    /// Source, flow and sender for one stream.
    fn stream_resources(
        &self,
        key: &StreamKey,
        ids: &[String],
        f: &OutputConfig,
        label: &str,
        active: bool,
    ) -> (Value, Value, Value) {
        let (src, flow, snd) = (
            self.stream_uid("source", ids),
            self.stream_uid("flow", ids),
            self.stream_uid("sender", ids),
        );
        let channels: Vec<Value> = (1..=f.channels)
            .map(|c| json!({"label": format!("Channel {c}")}))
            .collect();
        let source = self.with_version(src, json!({
            "id": src, "label": label, "description": format!("AES67 stream to {key}"), "tags": {}, "caps": {},
            "device_id": self.device_id(), "parents": [], "clock_name": "clk0",
            "format": "urn:x-nmos:format:audio", "channels": channels,
        }));
        let flow_v = self.with_version(flow, json!({
            "id": flow, "label": label, "description": format!("AES67 stream to {key}"), "tags": {},
            "source_id": src, "device_id": self.device_id(), "parents": [],
            "format": "urn:x-nmos:format:audio", "sample_rate": {"numerator": f.sample_rate},
            "media_type": "audio/L24", "bit_depth": 24,
        }));
        let transport = if key.address.is_multicast() {
            "urn:x-nmos:transport:rtp.mcast"
        } else {
            "urn:x-nmos:transport:rtp.ucast"
        };
        let sender = self.with_version(snd, json!({
            "id": snd, "label": label, "description": format!("AES67 stream to {key}"), "tags": {},
            "flow_id": flow, "transport": transport, "device_id": self.device_id(),
            "manifest_href": format!("{}/x-nmos/connection/v1.1/single/senders/{snd}/transportfile", self.base()),
            "interface_bindings": [self.cfg.interface],
            "subscription": {"receiver_id": null, "active": active},
        }));
        (source, flow_v, sender)
    }

    /// Every resource by type, in registration order.
    pub fn resources(&self) -> Vec<(&'static str, Value)> {
        let mut v = vec![("node", self.node()), ("device", self.device())];
        for (key, f, ids, sdp, sending) in self.engine.streams() {
            let label = sdp
                .lines()
                .find_map(|l| l.strip_prefix("s="))
                .unwrap_or("AES67")
                .to_string();
            let (s, fl, sn) = self.stream_resources(&key, &ids, &f, &label, sending);
            v.push(("source", s));
            v.push(("flow", fl));
            v.push(("sender", sn));
        }
        v
    }

    fn stream_for_sender(
        &self,
        id: &str,
    ) -> Option<(StreamKey, OutputConfig, Vec<String>, String, bool)> {
        self.engine
            .streams()
            .into_iter()
            .find(|s| self.stream_uid("sender", &s.2).to_string() == id)
    }

    fn transport_state(&self, key: &StreamKey, enabled: bool) -> Value {
        json!({
            "receiver_id": null,
            "master_enable": enabled,
            "activation": {"mode": null, "requested_time": null, "activation_time": null},
            "transport_params": [{
                "source_ip": self.ip.to_string(), "destination_ip": key.address.to_string(),
                "source_port": "auto", "destination_port": key.port, "rtp_enabled": enabled,
            }],
        })
    }

    fn enabled(&self, ids: &[String]) -> bool {
        ids.iter()
            .filter_map(|i| self.engine.output(i))
            .any(|o| !o.nmos_disabled.load(Ordering::Relaxed))
    }

    /// Applies a staged IS-05 change: enable/disable, and a new destination.
    fn activate(
        &self,
        key: StreamKey,
        ids: &[String],
        staged: &Value,
    ) -> Result<StreamKey, String> {
        let enable = staged["master_enable"].as_bool();
        let tp = &staged["transport_params"][0];
        let mut new_key = key;
        if let Some(ip) = tp["destination_ip"].as_str() {
            new_key.address = ip
                .parse()
                .map_err(|_| format!("destination_ip {ip} is not IPv4"))?;
        }
        if let Some(p) = tp["destination_port"].as_u64() {
            new_key.port =
                u16::try_from(p).map_err(|_| "destination_port out of range".to_string())?;
        }
        if new_key != key {
            for id in ids {
                let Some(o) = self.engine.output(id) else {
                    continue;
                };
                let mut c = o.cfg.clone();
                c.address = new_key.address;
                c.port = new_key.port;
                self.engine.upsert_output(id, c)?;
            }
        }
        if let Some(e) = enable.or(tp["rtp_enabled"].as_bool()) {
            for id in ids {
                if let Some(o) = self.engine.output(id) {
                    o.nmos_disabled.store(!e, Ordering::Relaxed);
                }
            }
            self.engine.version.fetch_add(1, Ordering::Relaxed);
        }
        Ok(new_key)
    }

    pub fn handle(
        self: &Arc<Self>,
        method: &Method,
        url: &str,
        body: &str,
    ) -> (u16, String, &'static str) {
        let path = url.split('?').next().unwrap_or("").trim_end_matches('/');
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let j = |v: Value| (200, v.to_string(), "application/json");
        let not_found = || {
            (
                404,
                json!({"code": 404, "error": "not found", "debug": null}).to_string(),
                "application/json",
            )
        };
        let list = |t: &str| -> Vec<Value> {
            self.resources()
                .into_iter()
                .filter(|r| r.0 == t)
                .map(|r| r.1)
                .collect()
        };
        match (method, parts.as_slice()) {
            (Method::Get, [""]) => j(json!(["x-nmos/"])),
            (Method::Get, ["x-nmos"]) => j(json!(["node/", "connection/"])),
            (Method::Get, ["x-nmos", "node"]) => j(json!(["v1.3/"])),
            (Method::Get, ["x-nmos", "node", "v1.3"]) => j(json!([
                "self/",
                "devices/",
                "sources/",
                "flows/",
                "senders/",
                "receivers/"
            ])),
            (Method::Get, ["x-nmos", "node", "v1.3", "self"]) => j(self.node()),
            (Method::Get, ["x-nmos", "node", "v1.3", "receivers"]) => j(json!([])),
            (Method::Get, ["x-nmos", "node", "v1.3", kind]) => match kind.strip_suffix('s') {
                Some(t @ ("device" | "source" | "flow" | "sender")) => j(json!(list(t))),
                _ => not_found(),
            },
            (Method::Get, ["x-nmos", "node", "v1.3", kind, id]) => {
                let t = kind.strip_suffix('s').unwrap_or("");
                list(t)
                    .into_iter()
                    .find(|r| r["id"] == *id)
                    .map(j)
                    .unwrap_or_else(not_found)
            }
            (Method::Get, ["x-nmos", "connection"]) => j(json!(["v1.1/"])),
            (Method::Get, ["x-nmos", "connection", "v1.1"]) => j(json!(["single/", "bulk/"])),
            (Method::Get, ["x-nmos", "connection", "v1.1", "single"]) => {
                j(json!(["senders/", "receivers/"]))
            }
            (Method::Get, ["x-nmos", "connection", "v1.1", "single", "receivers"]) => j(json!([])),
            (Method::Get, ["x-nmos", "connection", "v1.1", "single", "senders"]) => {
                let ids: Vec<String> = self
                    .engine
                    .streams()
                    .iter()
                    .map(|s| format!("{}/", self.stream_uid("sender", &s.2)))
                    .collect();
                j(json!(ids))
            }
            (_, ["x-nmos", "connection", "v1.1", "single", "senders", id, rest @ ..]) => {
                let Some((key, _f, ids, sdp, _)) = self.stream_for_sender(id) else {
                    return not_found();
                };
                let uid: Uuid = id.parse().unwrap_or_default();
                let enabled = self.enabled(&ids);
                match (method, rest) {
                    (Method::Get, []) => j(json!([
                        "constraints/",
                        "staged/",
                        "active/",
                        "transportfile/",
                        "transporttype/"
                    ])),
                    (Method::Get, ["transporttype"]) => j(json!("urn:x-nmos:transport:rtp")),
                    (Method::Get, ["constraints"]) => j(json!([{
                        "source_ip": {"enum": [self.ip.to_string()]}, "destination_ip": {}, "source_port": {},
                        "destination_port": {"minimum": 1, "maximum": 65535}, "rtp_enabled": {},
                    }])),
                    (Method::Get, ["active"]) => j(self.transport_state(&key, enabled)),
                    (Method::Get, ["staged"]) => {
                        let s = self.staged.lock().unwrap().get(&uid).cloned();
                        j(s.unwrap_or_else(|| self.transport_state(&key, enabled)))
                    }
                    (Method::Get, ["transportfile"]) => {
                        if enabled {
                            (200, sdp, "application/sdp")
                        } else {
                            not_found()
                        }
                    }
                    (Method::Patch, ["staged"]) => self.patch_staged(uid, key, &ids, enabled, body),
                    _ => (
                        405,
                        json!({"code": 405, "error": "method not allowed", "debug": null})
                            .to_string(),
                        "application/json",
                    ),
                }
            }
            _ => not_found(),
        }
    }

    fn patch_staged(
        self: &Arc<Self>,
        uid: Uuid,
        key: StreamKey,
        ids: &[String],
        enabled: bool,
        body: &str,
    ) -> (u16, String, &'static str) {
        let bad = |m: &str| {
            (
                400,
                json!({"code": 400, "error": m, "debug": null}).to_string(),
                "application/json",
            )
        };
        let patch: Value = match serde_json::from_str(body) {
            Ok(v @ Value::Object(_)) => v,
            _ => return bad("body must be a JSON object"),
        };
        let mut staged = self
            .staged
            .lock()
            .unwrap()
            .get(&uid)
            .cloned()
            .unwrap_or_else(|| self.transport_state(&key, enabled));
        if let Some(e) = patch.get("master_enable") {
            staged["master_enable"] = e.clone();
        }
        if let Some(r) = patch.get("receiver_id") {
            staged["receiver_id"] = r.clone();
        }
        if let Some(Value::Array(tps)) = patch.get("transport_params") {
            if let Some(Value::Object(tp)) = tps.first() {
                for (k, v) in tp {
                    if ![
                        "source_ip",
                        "destination_ip",
                        "source_port",
                        "destination_port",
                        "rtp_enabled",
                    ]
                    .contains(&k.as_str())
                    {
                        return bad(&format!("unknown transport parameter {k}"));
                    }
                    staged["transport_params"][0][k] = v.clone();
                }
            }
        }
        let mode = patch["activation"]["mode"].as_str().map(str::to_string);
        match mode.as_deref() {
            None => {
                self.staged.lock().unwrap().insert(uid, staged.clone());
                (200, staged.to_string(), "application/json")
            }
            Some("activate_immediate") => match self.activate(key, ids, &staged) {
                Ok(new_key) => {
                    self.staged.lock().unwrap().remove(&uid);
                    let mut out = staged.clone();
                    out["activation"] = json!({"mode": "activate_immediate", "requested_time": null, "activation_time": tai_version()});
                    let _ = new_key;
                    (200, out.to_string(), "application/json")
                }
                Err(e) => bad(&e),
            },
            Some(m @ ("activate_scheduled_relative" | "activate_scheduled_absolute")) => {
                let Some(t) = patch["activation"]["requested_time"].as_str() else {
                    return bad("requested_time is required");
                };
                let Some(at) = parse_tai(t) else {
                    return bad("requested_time is not seconds:nanoseconds");
                };
                let delay = if m.ends_with("relative") {
                    at
                } else {
                    let now = parse_tai(&tai_version()).unwrap_or_default();
                    at.saturating_sub(now)
                };
                let me = self.clone();
                let ids = ids.to_vec();
                let staged_c = staged.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(delay);
                    if let Err(e) = me.activate(key, &ids, &staged_c) {
                        log::warn(&format!("NMOS scheduled activation failed: {e}"));
                    }
                    me.staged.lock().unwrap().remove(&uid);
                });
                let mut out = staged;
                out["activation"] =
                    json!({"mode": m, "requested_time": t, "activation_time": null});
                (202, out.to_string(), "application/json")
            }
            Some(other) => bad(&format!("activation mode {other} is not supported")),
        }
    }

    pub fn serve(self: Arc<Self>, server: Server) {
        for mut req in server.incoming_requests() {
            let me = self.clone();
            std::thread::spawn(move || {
                let mut body = String::new();
                let _ = std::io::Read::read_to_string(&mut req.as_reader(), &mut body);
                let (status, text, ct) = me.handle(&req.method().clone(), req.url(), &body);
                let resp = Response::from_string(text)
                    .with_status_code(status)
                    .with_header(Header::from_bytes("Content-Type", ct).unwrap())
                    .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap());
                let _ = req.respond(resp);
            });
        }
    }

    /// Finds registries, registers, heartbeats, and keeps the registry in step.
    pub fn run_registration(self: Arc<Self>, mdns: mdns_sd::ServiceDaemon) {
        let browse = mdns.browse("_nmos-register._tcp.local.").ok();
        let browse_old = mdns.browse("_nmos-registration._tcp.local.").ok();
        let mut registries: BTreeMap<String, (u32, String)> = BTreeMap::new();
        let mut advertised = false;
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(3))
            .build();
        let mut current: Option<String> = None;
        let mut registered: HashMap<Uuid, String> = HashMap::new();
        let mut last_heartbeat = Instant::now() - Duration::from_secs(10);
        loop {
            for rx in [&browse, &browse_old].into_iter().flatten() {
                while let Ok(ev) = rx.try_recv() {
                    match ev {
                        mdns_sd::ServiceEvent::ServiceResolved(info) => {
                            let props = info.get_properties();
                            let proto = props.get_property_val_str("api_proto").unwrap_or("http");
                            let vers = props.get_property_val_str("api_ver").unwrap_or("v1.3");
                            if proto != "http" || !vers.split(',').any(|v| v == "v1.3") {
                                continue;
                            }
                            let pri: u32 = props
                                .get_property_val_str("pri")
                                .and_then(|p| p.parse().ok())
                                .unwrap_or(100);
                            if let Some(addr) = info.get_addresses_v4().iter().next() {
                                let url = format!(
                                    "http://{}:{}/x-nmos/registration/v1.3",
                                    addr,
                                    info.get_port()
                                );
                                registries.insert(info.get_fullname().to_string(), (pri, url));
                            }
                        }
                        mdns_sd::ServiceEvent::ServiceRemoved(_, name) => {
                            registries.remove(&name);
                        }
                        _ => {}
                    }
                }
            }
            let best = registries.values().min().map(|(_, u)| u.clone());
            if best != current {
                current = best.clone();
                registered.clear();
                if let Some(u) = &current {
                    log::info(&format!("NMOS registry {u}"));
                }
            }
            match &current {
                Some(reg) => {
                    if advertised {
                        let _ = mdns.unregister(&self.node_service_name());
                        advertised = false;
                    }
                    if let Err(e) =
                        self.sync_registry(&agent, reg, &mut registered, &mut last_heartbeat)
                    {
                        log::debug(&format!("NMOS registry {reg}: {e}"));
                        registered.clear();
                    }
                }
                None => {
                    if !advertised {
                        advertised = self.advertise(&mdns).is_ok();
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(1000));
        }
    }

    fn node_service_name(&self) -> String {
        format!(
            "{}._nmos-node._tcp.local.",
            self.cfg.label.replace('.', "-")
        )
    }

    fn advertise(&self, mdns: &mdns_sd::ServiceDaemon) -> Result<(), mdns_sd::Error> {
        let props = [
            ("api_proto", "http"),
            ("api_ver", "v1.3"),
            ("api_auth", "false"),
        ];
        let info = mdns_sd::ServiceInfo::new(
            "_nmos-node._tcp.local.",
            &self.cfg.label.replace('.', "-"),
            &format!("{}.local.", self.cfg.hostname),
            self.ip.to_string(),
            self.cfg.port,
            &props[..],
        )?;
        mdns.register(info)
    }

    fn sync_registry(
        &self,
        agent: &ureq::Agent,
        reg: &str,
        registered: &mut HashMap<Uuid, String>,
        last_heartbeat: &mut Instant,
    ) -> Result<(), String> {
        let resources = self.resources();
        // Register what is new or changed, in dependency order.
        for (kind, body) in &resources {
            let id: Uuid = body["id"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or_default();
            let ver = body["version"].as_str().unwrap_or("").to_string();
            if registered.get(&id) == Some(&ver) {
                continue;
            }
            let r = agent
                .post(&format!("{reg}/resource"))
                .set("Content-Type", "application/json")
                .send_string(&json!({"type": kind, "data": body}).to_string());
            match r {
                Ok(_) => {
                    registered.insert(id, ver);
                }
                Err(e) => return Err(format!("register {kind}: {e}")),
            }
        }
        // Withdraw what no longer exists (never the node, which goes last at shutdown).
        let live: std::collections::HashSet<Uuid> = resources
            .iter()
            .filter_map(|(_, b)| b["id"].as_str().and_then(|s| s.parse().ok()))
            .collect();
        let stale: Vec<(Uuid, &'static str)> = registered
            .keys()
            .filter(|id| !live.contains(id))
            .map(|id| (*id, "senders"))
            .collect();
        for (id, _) in stale {
            for kind in ["senders", "flows", "sources"] {
                let _ = agent.delete(&format!("{reg}/resource/{kind}/{id}")).call();
            }
            registered.remove(&id);
        }
        if last_heartbeat.elapsed() >= Duration::from_secs(5) {
            *last_heartbeat = Instant::now();
            match agent
                .post(&format!("{reg}/health/nodes/{}", self.node_id()))
                .call()
            {
                Ok(_) => {}
                Err(ureq::Error::Status(404, _)) => {
                    registered.clear();
                }
                Err(e) => return Err(format!("heartbeat: {e}")),
            }
        }
        Ok(())
    }
}

/// "seconds:nanoseconds" as a duration.
fn parse_tai(t: &str) -> Option<Duration> {
    let (s, n) = t.split_once(':')?;
    Some(Duration::new(s.parse().ok()?, n.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::Clock;
    use crate::stream::NetConfig;

    fn setup() -> Arc<Nmos> {
        let e = Engine::new(
            Arc::new(Clock::new()),
            NetConfig {
                iface_ip: Ipv4Addr::LOCALHOST,
                dscp_media: 34,
            },
        );
        let cfg: OutputConfig =
            serde_json::from_str(r#"{"name":"Lobby","address":"239.69.1.10","keep_alive":false}"#)
                .unwrap();
        e.upsert_output("lobby", cfg).unwrap();
        Nmos::new(
            e,
            NmosConfig {
                label: "ma-aes67".into(),
                hostname: "homeassistant".into(),
                port: 8891,
                interface: "eth0".into(),
                mac: Some("02-42-ac-11-00-02".into()),
                seed: "test".into(),
            },
        )
    }

    #[test]
    fn ids_and_versions_are_stable() {
        let n = setup();
        let a = n.node();
        let b = n.node();
        assert_eq!(a["id"], b["id"]);
        assert_eq!(a["version"], b["version"]);
        assert_eq!(n.id("node"), setup().id("node"));
        let kinds: Vec<&str> = n.resources().iter().map(|r| r.0).collect();
        assert_eq!(kinds, ["node", "device", "source", "flow", "sender"]);
    }

    #[test]
    fn node_api_lists_the_sender() {
        let n = setup();
        let (st, body, _) = n.handle(&Method::Get, "/x-nmos/node/v1.3/senders/", "");
        assert_eq!(st, 200);
        let senders: Value = serde_json::from_str(&body).unwrap();
        let s = &senders[0];
        assert_eq!(s["transport"], "urn:x-nmos:transport:rtp.mcast");
        assert_eq!(s["label"], "Lobby");
        let id = s["id"].as_str().unwrap();
        let (st, _, _) = n.handle(&Method::Get, &format!("/x-nmos/node/v1.3/senders/{id}"), "");
        assert_eq!(st, 200);
        let (st, sdp, ct) = n.handle(
            &Method::Get,
            &format!("/x-nmos/connection/v1.1/single/senders/{id}/transportfile"),
            "",
        );
        assert_eq!((st, ct), (200, "application/sdp"));
        assert!(sdp.contains("239.69.1.10"));
    }

    #[test]
    fn is05_disables_and_moves_a_sender() {
        let n = setup();
        let id = n.resources().iter().find(|r| r.0 == "sender").unwrap().1["id"]
            .as_str()
            .unwrap()
            .to_string();
        let base = format!("/x-nmos/connection/v1.1/single/senders/{id}");
        let (st, _, _) = n.handle(
            &Method::Patch,
            &format!("{base}/staged"),
            r#"{"master_enable": false, "activation": {"mode": "activate_immediate"}}"#,
        );
        assert_eq!(st, 200);
        let (_, active, _) = n.handle(&Method::Get, &format!("{base}/active"), "");
        assert_eq!(
            serde_json::from_str::<Value>(&active).unwrap()["master_enable"],
            false
        );
        let (st, _, _) = n.handle(&Method::Get, &format!("{base}/transportfile"), "");
        assert_eq!(st, 404);

        let (st, _, _) = n.handle(
            &Method::Patch,
            &format!("{base}/staged"),
            r#"{"master_enable": true, "transport_params": [{"destination_ip": "239.69.1.20", "destination_port": 5006}], "activation": {"mode": "activate_immediate"}}"#,
        );
        assert_eq!(st, 200);
        let o = n.engine.output("lobby").unwrap();
        assert_eq!(
            (o.cfg.address, o.cfg.port),
            (Ipv4Addr::new(239, 69, 1, 20), 5006)
        );
        let (st, _, _) = n.handle(
            &Method::Patch,
            &format!("{base}/staged"),
            r#"{"transport_params": [{"bogus": 1}]}"#,
        );
        assert_eq!(st, 400);
        n.engine.shutdown();
    }
}
