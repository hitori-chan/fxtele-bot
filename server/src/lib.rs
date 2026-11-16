//! Telegram link previews and Instant View for Facebook posts. AGENTS.md has the layout.
//! The binary in `main.rs` is the command line; everything else lives here so benchmarks
//! and tests can reach it.

pub mod app;
pub mod cache;
pub mod canary;
pub mod client_ip;
pub mod config;
pub mod cookies;
pub mod dashboard;
pub mod db;
pub mod extract;
pub mod limits;
pub mod net;
pub mod public;
pub mod render;
pub mod settings;
pub mod stats;
pub mod tasks;
pub mod telegram_ips;
