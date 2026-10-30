// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! The control API a Music Assistant player provider drives: JSON over HTTP.
//!
//! ```text
//! GET    /status                  clock and every output
//! GET    /outputs/{id}            one output's state
//! PUT    /outputs/{id}            create or change (body: the output's config)
//! DELETE /outputs/{id}
//! POST   /outputs/{id}/play       {"url": "...", "members": ["id", ...]}
//! POST   /outputs/{id}/stop
//! POST   /outputs/{id}/volume     {"level": 0..100, "muted": bool}
//! GET    /outputs/{id}/sdp        the stream's SDP
//! ```
//!
//! Outputs set up in the add-on's own options belong to it and can be played
//! and read here but not changed or removed.

use std::collections::HashSet;
use std::io::Read;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::engine::Engine;
use crate::log;
use crate::output::OutputConfig;

pub const API_VERSION: u32 = 1;

pub struct Api {
    pub engine: Arc<Engine>,
    pub token: Option<String>,
    /// Outputs from the add-on options.
    pub fixed: HashSet<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlayBody {
    url: String,
    #[serde(default)]
    members: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VolumeBody {
    level: Option<u8>,
    muted: Option<bool>,
}

pub struct Reply {
    pub status: u16,
    pub body: String,
    pub content_type: &'static str,
}

fn ok(v: Value) -> Reply {
    Reply { status: 200, body: v.to_string(), content_type: "application/json" }
}

fn err(status: u16, msg: impl Into<String>) -> Reply {
    Reply { status, body: json!({ "error": msg.into() }).to_string(), content_type: "application/json" }
}

fn parse<'a, T: Deserialize<'a>>(body: &'a str) -> Result<T, Reply> {
    serde_json::from_str(body).map_err(|e| err(400, format!("bad request body: {e}")))
}

impl Api {
    /// Handles one request; separate from the server so tests need no sockets.
    pub fn handle(&self, method: &Method, url: &str, auth: Option<&str>, body: &str) -> Reply {
        if let Some(t) = &self.token {
            if auth != Some(&format!("Bearer {t}")) {
                return err(401, "missing or wrong token");
            }
        }
        let path = url.split('?').next().unwrap_or("");
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        match (method, parts.as_slice()) {
            (Method::Get, ["status"]) => {
                let outputs: Vec<Value> = self
                    .engine
                    .output_ids()
                    .iter()
                    .filter_map(|id| self.engine.status(id))
                    .map(|s| serde_json::to_value(s).unwrap())
                    .collect();
                ok(json!({
                    "api_version": API_VERSION,
                    "version": env!("CARGO_PKG_VERSION"),
                    "interface": self.engine.net.iface_ip.to_string(),
                    "clock": self.engine.clock_status(),
                    "outputs": outputs,
                }))
            }
            (Method::Get, ["outputs", id]) => match self.engine.status(id) {
                Some(s) => ok(serde_json::to_value(s).unwrap()),
                None => err(404, format!("no output '{id}'")),
            },
            (Method::Put, ["outputs", id]) => {
                if self.fixed.contains(*id) {
                    return err(409, format!("'{id}' is set up in the add-on's options"));
                }
                let cfg: OutputConfig = match parse(body) {
                    Ok(c) => c,
                    Err(r) => return r,
                };
                match self.engine.upsert_output(id, cfg) {
                    Ok(()) => ok(serde_json::to_value(self.engine.status(id)).unwrap()),
                    Err(e) => err(422, e),
                }
            }
            (Method::Delete, ["outputs", id]) => {
                if self.fixed.contains(*id) {
                    return err(409, format!("'{id}' is set up in the add-on's options"));
                }
                if self.engine.remove_output(id) {
                    ok(json!({}))
                } else {
                    err(404, format!("no output '{id}'"))
                }
            }
            (Method::Post, ["outputs", id, "play"]) => {
                let b: PlayBody = match parse(body) {
                    Ok(b) => b,
                    Err(r) => return r,
                };
                if !(b.url.starts_with("http://") || b.url.starts_with("https://")) {
                    return err(400, "url must be http or https");
                }
                match self.engine.play_url(id, &b.url, &b.members) {
                    Ok(session) => ok(json!({ "session": session })),
                    Err(e) => err(422, e),
                }
            }
            (Method::Post, ["outputs", id, "stop"]) => match self.engine.stop(id) {
                Ok(()) => ok(json!({})),
                Err(e) => err(404, e),
            },
            (Method::Post, ["outputs", id, "volume"]) => {
                let b: VolumeBody = match parse(body) {
                    Ok(b) => b,
                    Err(r) => return r,
                };
                if b.level.is_some_and(|l| l > 100) {
                    return err(400, "level is 0 to 100");
                }
                match self.engine.set_volume(id, b.level, b.muted) {
                    Ok(()) => ok(json!({})),
                    Err(e) => err(404, e),
                }
            }
            (Method::Get, ["outputs", id, "sdp"]) => match self.engine.sdp_for_output(id) {
                Some(sdp) => Reply { status: 200, body: sdp, content_type: "application/sdp" },
                None => err(404, format!("no output '{id}'")),
            },
            _ => err(404, "no such endpoint"),
        }
    }

    pub fn serve(self: Arc<Self>, server: Server) {
        for mut req in server.incoming_requests() {
            let api = self.clone();
            // Requests are short; one thread each keeps a slow client from blocking the rest.
            std::thread::spawn(move || {
                let reply = api.handle_request(&mut req);
                let ct = Header::from_bytes("Content-Type", reply.content_type).unwrap();
                let resp = Response::from_string(reply.body).with_status_code(reply.status).with_header(ct);
                if let Err(e) = req.respond(resp) {
                    log::debug(&format!("api reply failed: {e}"));
                }
            });
        }
    }

    fn handle_request(&self, req: &mut Request) -> Reply {
        let mut body = String::new();
        if req.as_reader().take(1 << 20).read_to_string(&mut body).is_err() {
            return err(400, "body is not UTF-8");
        }
        let auth = req
            .headers()
            .iter()
            .find(|h| h.field.equiv("Authorization"))
            .map(|h| h.value.as_str().to_string());
        let method = req.method().clone();
        let url = req.url().to_string();
        self.handle(&method, &url, auth.as_deref(), &body)
    }
}
