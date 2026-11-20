//! `tbd`: a localhost WebSocket hub. The Thunderbird extension connects as the `extension` peer
//! and executes operations; `tb` and the MCP server connect as `client` peers and send them.

mod auth;
mod hub;
mod server;

pub use server::{Config, RunningServer, start};
