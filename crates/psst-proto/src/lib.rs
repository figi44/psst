pub mod api;
pub mod endpoint;
pub mod types;
pub mod ws;

pub use endpoint::{Endpoint, EndpointParseError, Token};
pub use types::*;

/// The server speaks only its own version and refuses any other with `error protocol`, whose
/// [`ws::ProtocolDetail`] tells the client which side to update. Bump it only for what the other
/// side can't ignore: a new optional field doesn't.
pub const PROTOCOL_VERSION: u32 = 1;

pub const DEFAULT_PORT: u16 = 7341;

pub const DEFAULT_SOCKET: &str = "/run/psst/client.sock";

pub const SAMPLE_RATE: u32 = 16_000;

pub const FRAME_MS: u32 = 20;

pub const FRAME_SAMPLES: usize = 320;

pub const FRAME_BYTES: usize = FRAME_SAMPLES * 2;

/// Chosen by the client.
pub type SessionId = uuid::Uuid;

pub type HistoryId = u64;

/// `local:<user>` for socket users, or the id issued at pairing.
pub type DeviceId = String;
