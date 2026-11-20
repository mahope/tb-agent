//! Wire protocol shared by `tbd` (daemon), `tb` (CLI and MCP) and the Thunderbird extension.
//!
//! The protocol is documented in `docs/protocol.md`. Frames are JSON objects sent as
//! WebSocket text messages.

pub mod connection;
pub mod frame;
pub mod ops;

pub use connection::ConnectionInfo;
pub use frame::{Capabilities, ErrorBody, Frame, Hello, Role, Welcome, code};
pub use ops::{Access, OpSpec, find_op, ops, validate_params};

/// Protocol version spoken by this build. Bumped on incompatible changes.
pub const PROTOCOL_VERSION: u32 = 2;

/// Largest single WebSocket message the daemon accepts or sends (16 MiB).
/// Bigger results must be streamed as `chunk` frames.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Largest result the daemon reassembles or relays from chunks (256 MiB).
pub const MAX_STREAM_BYTES: usize = 256 * 1024 * 1024;

/// Name of the WebSocket endpoint on the daemon.
pub const WS_PATH: &str = "/ws";
