//! The server's HTTP API for the CLI (`psst dict`, `psst ui`): the server `client.toml` names, its
//! socket on the server's machine and TCP with the token from `psst pair` elsewhere. The daemon
//! isn't involved.

use anyhow::{Context, anyhow};
use psst_client::api::{ApiCallError, ApiClient};
use psst_config::{ClientConfig, ConfigFile};
use psst_proto::Endpoint;

use crate::pair::Credentials;

/// The server from `client.toml`, with this device's token over TCP.
pub fn connect() -> anyhow::Result<ApiClient> {
    let path = ClientConfig::default_path().ok_or_else(|| anyhow!("cannot find client.toml"))?;
    let server = ClientConfig::load_or_default(&path)?.server;
    let token = match &server {
        Endpoint::Unix(_) => None,
        Endpoint::Tcp { .. } => {
            let credentials = crate::state_dir()?.join("credentials");
            let creds = Credentials::load(&credentials)
                .with_context(|| format!("cannot read {}", credentials.display()))?
                .ok_or_else(|| anyhow!("this device isn't paired with {server}; run `psst pair {server} <code>`"))?;
            Some(creds.token)
        }
    };
    Ok(ApiClient::new(server, token))
}

/// A failed call, said for people: the server's own reason when it gave one.
pub fn describe(err: ApiCallError, endpoint: &Endpoint) -> anyhow::Error {
    match err {
        ApiCallError::Api { error, .. } => anyhow!("{}", error.message),
        ApiCallError::Connect { .. } => anyhow!("{err}; is psst-server running at {endpoint}?"),
        other => anyhow!(other),
    }
}
