//! Wire types shared by the psst server and its clients.
//!
//! - [`types`]: the shared result, timing and error types (plan 5.6).
//! - [`ws`]: the client ↔ server WebSocket messages on `/v1/client` (plan 5.1).
//! - [`api`]: the HTTP routes and their request and response bodies (plan 5.2).
//! - [`endpoint`]: where a client finds the server, and the device token it presents.
//!
//! `docs/protocol.md` is the prose version of this crate; a change to one updates the other.

pub mod api;
pub mod endpoint;
pub mod types;
pub mod ws;

pub use endpoint::{Endpoint, EndpointParseError, Token};
pub use types::*;

/// Version of the client ↔ server protocol, sent in `hello`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Default TCP port of the server.
pub const DEFAULT_PORT: u16 = 7341;

/// Default path of the server's Unix socket on the machine that runs it.
pub const DEFAULT_SOCKET: &str = "/run/psst/client.sock";

/// Sample rate of all audio on the wire, in Hz.
pub const SAMPLE_RATE: u32 = 16_000;

/// Length of one binary audio frame, in milliseconds.
pub const FRAME_MS: u32 = 20;

/// Samples in one binary audio frame (20 ms at 16 kHz).
pub const FRAME_SAMPLES: usize = 320;

/// Bytes in one binary audio frame (`pcm_s16le`).
pub const FRAME_BYTES: usize = FRAME_SAMPLES * 2;

/// Unique identity of a dictation, chosen by the client.
pub type SessionId = uuid::Uuid;

/// Id of a session's history row, assigned by the server at `start`.
pub type HistoryId = u64;

/// Id of a device: `local:<user>` for socket users, or the id issued at pairing.
pub type DeviceId = String;
