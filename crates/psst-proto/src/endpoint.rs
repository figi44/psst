use std::{fmt, path::PathBuf, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::DEFAULT_PORT;

/// Where a client reaches the server.
///
/// Written as an absolute path for the Unix socket (`/run/psst/client.sock`), or as
/// `http://host[:port]` for TCP (port 7341 when omitted).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Endpoint {
    Unix(PathBuf),
    Tcp { host: String, port: u16 },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid server address `{0}`: expected an absolute socket path or http://host[:port]")]
pub struct EndpointParseError(String);

impl FromStr for Endpoint {
    type Err = EndpointParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || EndpointParseError(s.to_owned());
        if s.starts_with('/') {
            return Ok(Self::Unix(PathBuf::from(s)));
        }
        let authority = s.strip_prefix("http://").ok_or_else(err)?;
        let authority = authority.strip_suffix('/').unwrap_or(authority);
        if authority.contains(['/', '?', '#', '@']) {
            return Err(err());
        }
        let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
            // IPv6 literal: [addr] or [addr]:port
            let (host, after) = rest.split_once(']').ok_or_else(err)?;
            match after {
                "" => (host, None),
                _ => (host, Some(after.strip_prefix(':').ok_or_else(err)?)),
            }
        } else {
            match authority.split_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (authority, None),
            }
        };
        if host.is_empty() {
            return Err(err());
        }
        let port = match port {
            Some(port) => port.parse().map_err(|_| err())?,
            None => DEFAULT_PORT,
        };
        Ok(Self::Tcp { host: host.to_owned(), port })
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "{}", path.display()),
            Self::Tcp { host, port } if host.contains(':') => write!(f, "http://[{host}]:{port}"),
            Self::Tcp { host, port } => write!(f, "http://{host}:{port}"),
        }
    }
}

impl TryFrom<String> for Endpoint {
    type Error = EndpointParseError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Endpoint> for String {
    fn from(endpoint: Endpoint) -> Self {
        endpoint.to_string()
    }
}

/// A device token, sent as `Authorization: Bearer <token>`. Its `Debug` output is redacted.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Token(String);

impl Token {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}
