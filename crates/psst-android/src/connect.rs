use psst_client::{
    Client, ClientError, Outdated,
    api::{ApiCallError, ApiClient, Method},
};
use psst_proto::{
    Endpoint, ErrorCode, Token,
    api::{PairRedeem, PairRedeemed, routes},
};
use tracing::info;

use crate::Link;

const CODE_HINT: &str = "pair it with a code from psst-server pair";

/// Why the link is down, in the words the app shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    /// `Offline`, `NeedsPairing`, `UpdateApp` or `UpdateServer`.
    pub link: Link,
    pub message: String,
}

impl Failure {
    pub fn offline(message: impl Into<String>) -> Failure {
        Failure { link: Link::Offline, message: message.into() }
    }

    pub fn no_answer() -> Failure {
        Failure::offline("The server did not answer in time; check this phone's connection.")
    }

    fn from_client(err: &ClientError, had_token: bool) -> Failure {
        match err {
            ClientError::Incompatible(incompatible) => {
                let version = incompatible.server_version.as_ref().map(|v| format!(" ({v})")).unwrap_or_default();
                match incompatible.outdated() {
                    Outdated::Client => Failure {
                        link: Link::UpdateApp,
                        message: format!("The server runs a newer psst{version}: update this app."),
                    },
                    Outdated::Server => Failure {
                        link: Link::UpdateServer,
                        message: format!("The server runs an older psst{version}: update the server."),
                    },
                }
            }
            ClientError::Refused(refusal) if refusal.code == ErrorCode::Unauthorized => Failure {
                link: Link::NeedsPairing,
                message: if had_token {
                    no_longer_accepted()
                } else {
                    format!("The server does not know this phone: {CODE_HINT}.")
                },
            },
            ClientError::Refused(refusal) => {
                Failure::offline(format!("The server refused the connection: {}.", sentence(&refusal.message)))
            }
            ClientError::Connect { endpoint, source } => unreachable(endpoint, source),
            ClientError::Protocol(_) => not_psst(),
            ClientError::Timeout => Failure::no_answer(),
            ClientError::Closed => Failure::offline("The server closed the connection; try again."),
        }
    }
}

pub(crate) fn no_longer_accepted() -> String {
    format!("The server no longer accepts this phone: {CODE_HINT}.")
}

pub(crate) fn unreachable(endpoint: &Endpoint, source: &std::io::Error) -> Failure {
    // std has no ErrorKind for a name that doesn't resolve: its message is the only sign.
    if let Endpoint::Tcp { host, .. } = endpoint
        && source.to_string().starts_with("failed to lookup address information")
    {
        return Failure::offline(format!(
            "This phone cannot find {host}. If the server is on your tailnet, turn on Tailscale, and if \
             Tailscale's app-based split tunneling is on, let psst use Tailscale in its settings. Otherwise, \
             check the address."
        ));
    }
    Failure::offline(format!("Cannot reach the server ({source}); check the address and this phone's connection."))
}

pub(crate) fn not_psst() -> Failure {
    Failure::offline("That address does not answer like a psst server; check it.")
}

pub(crate) fn sentence(text: &str) -> &str {
    text.trim().trim_end_matches('.')
}

pub(crate) enum Learned {
    /// Saved only if `server` is still the configured one.
    Paired {
        server: Endpoint,
        paired: PairRedeemed,
    },
    TokenRefused(Token),
}

/// The desktop daemon's: pairs by tailnet identity when there is no token, or when the server
/// refuses the one there is (it was revoked, or the server lost its database). A token refused
/// before is tried after pairing, not before, so that a slow link doesn't spend the attempt on a
/// refusal it already knows.
pub(crate) async fn connect_or_pair(
    endpoint: &Endpoint,
    saved: Option<Token>,
    refused_before: bool,
    learned: &impl Fn(Learned),
) -> Result<Client, Failure> {
    let had_token = saved.is_some();
    let mut token = saved.clone();
    if token.is_none() || refused_before {
        token = pair_itself(endpoint, learned).await.or(token);
    }
    let mut result = Client::connect(endpoint, token.as_ref()).await;
    if let Some(saved) = saved
        && matches!(&result, Err(ClientError::Refused(r)) if r.code == ErrorCode::Unauthorized)
        && token.as_ref() == Some(&saved)
    {
        learned(Learned::TokenRefused(saved));
        if !refused_before && let Some(paired) = pair_itself(endpoint, learned).await {
            result = Client::connect(endpoint, Some(&paired)).await;
        }
    }
    result.map_err(|err| Failure::from_client(&err, had_token))
}

async fn pair_itself(endpoint: &Endpoint, learned: &impl Fn(Learned)) -> Option<Token> {
    let api = ApiClient::new(endpoint.clone(), None);
    match api.call::<PairRedeemed>(Method::Post, routes::PAIR_AUTO, None::<&()>).await {
        Ok(paired) => {
            info!("paired with {endpoint} as device {} by this phone's tailnet identity", paired.device_id);
            let token = paired.token.clone();
            learned(Learned::Paired { server: endpoint.clone(), paired });
            Some(token)
        }
        Err(err) => {
            let reason = match err {
                ApiCallError::Api { error, .. } => error.message,
                other => other.to_string(),
            };
            info!("cannot pair by tailnet identity: {reason}");
            None
        }
    }
}

pub(crate) async fn pair_with_code(
    endpoint: &Endpoint,
    code: &str,
    name: &str,
    learned: &impl Fn(Learned),
) -> Result<Client, Failure> {
    let paired = redeem(endpoint, code, name).await?;
    info!("paired with {endpoint} as device {} with a code", paired.device_id);
    let token = paired.token.clone();
    learned(Learned::Paired { server: endpoint.clone(), paired });
    Client::connect(endpoint, Some(&token)).await.map_err(|err| Failure::from_client(&err, true))
}

async fn redeem(endpoint: &Endpoint, code: &str, name: &str) -> Result<PairRedeemed, Failure> {
    let api = ApiClient::new(endpoint.clone(), None);
    let request = PairRedeem { code: code.trim().into(), name: name.into() };
    api.call(Method::Post, routes::PAIR_REDEEM, Some(&request)).await.map_err(|err| match err {
        ApiCallError::Api { error, .. } => Failure {
            link: Link::NeedsPairing,
            message: format!("The server refused the code: {}.", sentence(&error.message)),
        },
        ApiCallError::Connect { endpoint, source } => unreachable(&endpoint, &source),
        ApiCallError::Timeout(_) => Failure::no_answer(),
        ApiCallError::Protocol { .. } => not_psst(),
    })
}
