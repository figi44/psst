//! Just enough HTTPS for OpenAI. One connection per host stays open between requests, so a lab
//! run times the request rather than a TCP and TLS handshake, as a running server would.

use std::{collections::HashMap, fmt, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use http_body_util::Full;
use hyper::{
    Method, Request, Response, Uri,
    body::{Bytes, Incoming},
    client::conn::http1::SendRequest,
    header::{self, HeaderName},
};
use hyper_util::rt::TokioIo;
use tokio::{net::TcpStream, sync::Mutex};
use tokio_rustls::{
    TlsConnector,
    client::TlsStream,
    rustls::{ClientConfig, RootCertStore, crypto::ring, pki_types::ServerName},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Https {
    tls: TlsConnector,
    idle: Mutex<HashMap<String, SendRequest<Full<Bytes>>>>,
}

impl fmt::Debug for Https {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Https").finish_non_exhaustive()
    }
}

impl Https {
    pub fn new() -> Https {
        let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let config = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("ring supports TLS 1.2 and 1.3")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Https { tls: TlsConnector::from(Arc::new(config)), idle: Mutex::default() }
    }

    pub async fn send(
        &self,
        method: Method,
        url: &str,
        headers: &[(HeaderName, String)],
        body: Bytes,
    ) -> Result<Response<Incoming>> {
        let uri: Uri = url.parse().with_context(|| format!("{url} isn't a URL"))?;
        if uri.scheme_str() != Some("https") {
            bail!("{url} isn't https");
        }
        let host = uri.host().ok_or_else(|| anyhow!("{url} has no host"))?.to_owned();
        let port = uri.port_u16().unwrap_or(443);
        let origin = format!("{host}:{port}");
        let path = uri.path_and_query().map_or("/", |p| p.as_str()).to_owned();
        let request = || {
            let mut request = Request::builder().method(method.clone()).uri(path.as_str()).header(header::HOST, &host);
            for (name, value) in headers {
                request = request.header(name, value);
            }
            request.body(Full::new(body.clone())).context("a request builds")
        };

        let reused = self.idle.lock().await.remove(&origin);
        if let Some(mut sender) = reused
            && sender.ready().await.is_ok()
        {
            // The server may have closed the idle connection just as the request went out, so a
            // failure here gets one more try on a fresh connection.
            if let Ok(response) = sender.send_request(request()?).await {
                self.idle.lock().await.insert(origin, sender);
                return Ok(response);
            }
        }
        let mut sender = self.connect(&host, port).await?;
        let response = sender.send_request(request()?).await.with_context(|| format!("{method} {url}"))?;
        self.idle.lock().await.insert(origin, sender);
        Ok(response)
    }

    pub async fn tls(&self, host: &str, port: u16) -> Result<TlsStream<TcpStream>> {
        let timeout = || anyhow!("no connection to {host} within {CONNECT_TIMEOUT:?}");
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
            .await
            .map_err(|_| timeout())?
            .with_context(|| format!("cannot connect to {host}"))?;
        tcp.set_nodelay(true)?;
        let name = ServerName::try_from(host.to_owned())?;
        tokio::time::timeout(CONNECT_TIMEOUT, self.tls.connect(name, tcp))
            .await
            .map_err(|_| timeout())?
            .with_context(|| format!("TLS with {host} failed"))
    }

    /// Sends requests for `origin` (`host:port`) down `io` instead of a TLS connection.
    #[cfg(test)]
    pub async fn connect_to(&self, origin: &str, io: tokio::io::DuplexStream) {
        let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await.unwrap();
        tokio::spawn(connection);
        self.idle.lock().await.insert(origin.to_owned(), sender);
    }

    async fn connect(&self, host: &str, port: u16) -> Result<SendRequest<Full<Bytes>>> {
        let tls = self.tls(host, port).await?;
        let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls)).await?;
        let host = host.to_owned();
        tokio::spawn(async move {
            if let Err(err) = connection.await {
                tracing::debug!("connection to {host} ended: {err}");
            }
        });
        Ok(sender)
    }
}
