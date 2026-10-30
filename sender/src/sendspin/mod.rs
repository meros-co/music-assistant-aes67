// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! Each configured output as a Sendspin player, so stock Music Assistant finds
//! and plays to it with no provider of ours.
//!
//! One WebSocket listener serves every output at its own path
//! (`/sendspin/<id>`); each output is advertised as its own
//! `_sendspin._tcp` service with that path, and keeps its own identity.

pub mod client;
pub mod secure;
pub mod timefilter;

use std::collections::HashMap;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::http::StatusCode;

use crate::engine::Engine;
use crate::log;
use client::{Admission, ClientCtx};
use secure::Identity;

pub const DEFAULT_PORT: u16 = 8928;
pub const SERVICE: &str = "_sendspin._tcp.local.";

pub struct SendspinConfig {
    pub port: u16,
    pub data_dir: PathBuf,
    pub hostname: String,
    pub ip: Ipv4Addr,
    pub mac: Option<String>,
    pub receiver_latency_us: i64,
}

pub struct Sendspin {
    cfg: SendspinConfig,
    engine: Arc<Engine>,
    clients: Mutex<HashMap<String, Arc<ClientCtx>>>,
    shutdown: Arc<AtomicBool>,
}

pub fn path_for(id: &str) -> String {
    format!("/sendspin/{id}")
}

impl Sendspin {
    pub fn new(cfg: SendspinConfig, engine: Arc<Engine>, shutdown: Arc<AtomicBool>) -> Arc<Self> {
        Arc::new(Sendspin { cfg, engine, clients: Mutex::new(HashMap::new()), shutdown })
    }

    /// Makes `id` (an engine output) a Sendspin player. Restores its identity and volume.
    pub fn add_output(&self, id: &str) -> std::io::Result<Arc<ClientCtx>> {
        let identity = Identity::load_or_create(&self.cfg.data_dir.join(format!("{id}.json")))?;
        if let Some(o) = self.engine.output(id) {
            o.set_volume(identity.volume);
            o.set_muted(identity.muted);
        }
        log::info(&format!("sendspin {id}: client id {}; pairing token {}", identity.client_id(), identity.pairing_token()));
        let ctx = Arc::new(ClientCtx {
            engine: self.engine.clone(),
            output_id: id.to_string(),
            identity: Mutex::new(identity),
            manufacturer: "Meros".into(),
            product: "AES67 output".into(),
            mac: self.cfg.mac.clone(),
            receiver_latency_us: self.cfg.receiver_latency_us,
            admission: Admission::default(),
            shutdown: self.shutdown.clone(),
        });
        self.clients.lock().unwrap().insert(id.to_string(), ctx.clone());
        Ok(ctx)
    }

    pub fn client(&self, id: &str) -> Option<Arc<ClientCtx>> {
        self.clients.lock().unwrap().get(id).cloned()
    }

    /// Advertises every output over mDNS.
    pub fn advertise(&self, mdns: &mdns_sd::ServiceDaemon) {
        for (id, ctx) in self.clients.lock().unwrap().iter() {
            let name = ctx.output().map(|o| o.cfg.name.clone()).unwrap_or_else(|| id.clone());
            let path = path_for(id);
            let props = [("path", path.as_str()), ("name", name.as_str())];
            let instance = format!("{} {}", self.cfg.hostname, id);
            match mdns_sd::ServiceInfo::new(
                SERVICE,
                &instance,
                &format!("{}.local.", self.cfg.hostname),
                self.cfg.ip.to_string(),
                self.cfg.port,
                &props[..],
            ) {
                Ok(info) => {
                    if let Err(e) = mdns.register(info) {
                        log::error(&format!("sendspin {id}: mDNS: {e}"));
                    }
                }
                Err(e) => log::error(&format!("sendspin {id}: mDNS: {e}")),
            }
        }
    }

    pub fn bind(&self) -> std::io::Result<TcpListener> {
        TcpListener::bind((Ipv4Addr::UNSPECIFIED, self.cfg.port))
    }

    pub fn serve(self: Arc<Self>, listener: TcpListener) {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let me = self.clone();
            std::thread::spawn(move || me.accept(stream));
        }
    }

    fn accept(&self, stream: TcpStream) {
        let _ = stream.set_nodelay(true);
        let mut chosen: Option<Arc<ClientCtx>> = None;
        let callback = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
            if req.headers().contains_key("origin") {
                return Err(reject(StatusCode::FORBIDDEN));
            }
            let id = req.uri().path().strip_prefix("/sendspin/").unwrap_or("");
            match self.client(id) {
                Some(c) => {
                    chosen = Some(c);
                    Ok(resp)
                }
                None => Err(reject(StatusCode::NOT_FOUND)),
            }
        };
        let ws = match tungstenite::accept_hdr(stream, callback) {
            Ok(ws) => ws,
            Err(e) => {
                log::debug(&format!("sendspin: upgrade refused: {e}"));
                return;
            }
        };
        if let Some(ctx) = chosen {
            client::run(&ctx, ws);
        }
    }
}

fn reject(status: StatusCode) -> ErrorResponse {
    let mut r = ErrorResponse::new(None);
    *r.status_mut() = status;
    r
}
