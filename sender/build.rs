// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! Compiles the parts of libaes67 the sender uses from the vendored submodule:
//! the packet arithmetic and SDP. Sockets, pacing and SAP are the sender's own.

fn main() {
    let root = std::path::Path::new("../vendor/aes67/aes67");
    let mut build = cc::Build::new();
    build.include(root.join("include")).std("c11");
    for file in ["rtp.c", "sdp.c"] {
        let path = root.join("src").join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        build.file(path);
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        build.define("_CRT_SECURE_NO_WARNINGS", None);
    }
    build.compile("aes67");
}
