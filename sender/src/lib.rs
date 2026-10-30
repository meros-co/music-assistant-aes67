// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! Sends Music Assistant's audio to AES67 networks, timed by PTP.

pub mod api;
pub mod clock;
pub mod engine;
pub mod ffi;
pub mod log;
pub mod output;
pub mod pmc;
pub mod ptp;
pub mod sap;
pub mod stream;
pub mod timeline;
pub mod wav;
