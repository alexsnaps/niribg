// SPDX-License-Identifier: GPL-3.0-or-later
//! `niribg` — wallpaper daemon for the niri Wayland compositor, with blur on
//! overview.
//!
//! This crate is split into a thin binary ([`main`](../main/index.html)) and
//! this library so the daemon's command handling can be exercised in tests
//! without a live Wayland connection. See `DESIGN.md` for the full plan.

pub mod client;
pub mod color;
pub mod config;
pub mod daemon;
pub mod paths;
pub mod proto;
pub mod state;

/// Build version reported over the control socket (`{"cmd":"version"}`), used
/// to detect a stale client talking to a newer daemon in a mixed install.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
