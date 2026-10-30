// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! Plain log lines on stderr, which the Home Assistant add-on log shows.

use std::sync::atomic::{AtomicBool, Ordering};

static DEBUG: AtomicBool = AtomicBool::new(false);

pub fn set_debug(on: bool) {
    DEBUG.store(on, Ordering::Relaxed);
}

pub fn debug(msg: &str) {
    if DEBUG.load(Ordering::Relaxed) {
        eprintln!("[debug] {msg}");
    }
}

pub fn info(msg: &str) {
    eprintln!("[info] {msg}");
}

pub fn warn(msg: &str) {
    eprintln!("[warn] {msg}");
}

pub fn error(msg: &str) {
    eprintln!("[error] {msg}");
}
