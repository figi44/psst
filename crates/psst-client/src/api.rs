//! The HTTP API (`/api/*`) at the same endpoint as `/v1/client`: the Unix socket on the
//! server's machine, TCP with the device token elsewhere. One request per connection, which is all
//! the CLI needs. Like the rest of this crate, it reports problems and never retries.

use std::{io, time::Duration};

use psst_proto::{Endpoint, Token, api::ApiError};
use serde::{Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpStream, UnixStream},
};

/// How long one request may take, from connecting to the end of the answer.
pub const API_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum ApiCallError {
    #[error("cannot connect to {endpoint}: {source}")]
    Connect { endpoint: Endpoint, source: io::Error },
    /// The server answered with an error status and an [`ApiError`].
    #[error("{} (HTTP {status})", .error.message)]
    Api { status: u16, error: ApiError },
    #[error("no answer from {0} within {API_TIMEOUT:?}")]
    Timeout(Endpoint),
    #[error("unexpected answer from {endpoint}: {message}")]
    Protocol { endpoint: Endpoint, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }
}

/// Calls `/api/*` routes at one endpoint.
#[derive(Debug, Clone)]
pub struct ApiClient {
    endpoint: Endpoint,
    token: Option<Token>,
}

impl ApiClient {
    /// `token` is the device token, required over TCP and not sent over the socket.
    pub fn new(endpoint: Endpoint, token: Option<Token>) -> Self {
        Self { endpoint, token }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Sends `body` as JSON (none when `None`) and parses the JSON answer.
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&(impl Serialize + ?Sized)>,
    ) -> Result<T, ApiCallError> {
        let body = body.map(|body| serde_json::to_vec(body).expect("API bodies serialise"));
        let answer = self.request(method, path, body).await?;
        serde_json::from_slice(&answer)
            .map_err(|err| ApiCallError::Protocol { endpoint: self.endpoint.clone(), message: err.to_string() })
    }

    /// Like [`ApiClient::call`], for routes that answer with no body (`204 No Content`).
    pub async fn call_empty(
        &self,
        method: Method,
        path: &str,
        body: Option<&(impl Serialize + ?Sized)>,
    ) -> Result<(), ApiCallError> {
        let body = body.map(|body| serde_json::to_vec(body).expect("API bodies serialise"));
        self.request(method, path, body).await.map(drop)
    }

    /// One exchange; the body of a successful answer.
    async fn request(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> Result<Vec<u8>, ApiCallError> {
        tokio::time::timeout(API_TIMEOUT, self.exchange(method, path, body))
            .await
            .unwrap_or_else(|_| Err(ApiCallError::Timeout(self.endpoint.clone())))
    }

    async fn exchange(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> Result<Vec<u8>, ApiCallError> {
        let connect_err = |source| ApiCallError::Connect { endpoint: self.endpoint.clone(), source };
        let mut head = String::new();
        let raw = match &self.endpoint {
            Endpoint::Unix(socket) => {
                head.push_str(&format!("{} {path} HTTP/1.1\r\nHost: localhost\r\n", method.as_str()));
                let stream = UnixStream::connect(socket).await.map_err(connect_err)?;
                self.send(stream, head, body).await
            }
            Endpoint::Tcp { host, port } => {
                let authority = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
                head.push_str(&format!("{} {path} HTTP/1.1\r\nHost: {authority}\r\n", method.as_str()));
                if let Some(token) = &self.token {
                    head.push_str(&format!("Authorization: Bearer {}\r\n", token.as_str()));
                }
                let stream = TcpStream::connect((host.as_str(), *port)).await.map_err(connect_err)?;
                stream.set_nodelay(true).map_err(connect_err)?;
                self.send(stream, head, body).await
            }
        }
        .map_err(connect_err)?;
        let (status, body) = parse_response(&raw)
            .map_err(|message| ApiCallError::Protocol { endpoint: self.endpoint.clone(), message })?;
        if (200..300).contains(&status) {
            return Ok(body);
        }
        match serde_json::from_slice::<ApiError>(&body) {
            Ok(error) => Err(ApiCallError::Api { status, error }),
            Err(_) => Err(ApiCallError::Protocol {
                endpoint: self.endpoint.clone(),
                message: format!("HTTP {status}: {}", String::from_utf8_lossy(&body)),
            }),
        }
    }

    /// Writes the request and reads the whole answer; the server closes after it.
    async fn send(
        &self,
        mut stream: impl AsyncRead + AsyncWrite + Unpin,
        mut head: String,
        body: Option<Vec<u8>>,
    ) -> io::Result<Vec<u8>> {
        let body = body.unwrap_or_default();
        if !body.is_empty() {
            head.push_str("Content-Type: application/json\r\n");
        }
        head.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()));
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(&body).await?;
        stream.flush().await?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await?;
        Ok(raw)
    }
}

/// The status and body of a complete HTTP/1.1 response.
fn parse_response(raw: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut response = httparse::Response::new(&mut headers);
    let head_len = match response.parse(raw) {
        Ok(httparse::Status::Complete(len)) => len,
        Ok(httparse::Status::Partial) => return Err("the answer ended early".into()),
        Err(err) => return Err(format!("malformed HTTP: {err}")),
    };
    let status = response.code.ok_or("no status")?;
    let header = |name: &str| {
        response.headers.iter().find(|h| h.name.eq_ignore_ascii_case(name)).map(|h| String::from_utf8_lossy(h.value))
    };
    let rest = &raw[head_len..];
    let body = if header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        dechunk(rest).ok_or("a malformed chunked body")?
    } else if let Some(len) = header("content-length") {
        let len: usize = len.trim().parse().map_err(|_| "a malformed Content-Length")?;
        rest.get(..len).ok_or("the body ended early")?.to_vec()
    } else {
        rest.to_vec()
    };
    Ok((status, body))
}

fn dechunk(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n")?;
        let size_text = std::str::from_utf8(&body[..line_end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(body.get(..size)?);
        body = body.get(size + 2..)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_come_by_length_chunks_or_until_the_end() {
        let by_length = b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\n\r\n{\"a\":1}";
        assert_eq!(parse_response(by_length).unwrap(), (200, b"{\"a\":1}".to_vec()));
        let chunked =
            b"HTTP/1.1 400 Bad Request\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked).unwrap(), (400, b"{\"a\":1}".to_vec()));
        assert_eq!(parse_response(b"HTTP/1.1 204 No Content\r\n\r\n").unwrap(), (204, Vec::new()));
        assert!(parse_response(b"HTTP/1.1 200 OK\r\ncontent-length: 9\r\n\r\n{}").is_err());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\n").is_err());
    }
}
