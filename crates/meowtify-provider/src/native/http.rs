//! A small HTTP/1.1 client for the in-process providers.
//!
//! The desktop providers shell out to `curl`; iOS cannot. This client speaks enough
//! HTTP for the APIs we use: GET and POST, redirects, `Content-Length` and chunked
//! bodies, range requests, and the proxy setting from [`crate::net`]. TLS is rustls
//! with the ring provider, so there is no OpenSSL and nothing to compile with a C
//! toolchain beyond what the iOS build already carries for webrtc.
//!
//! Every response is read in small pieces; nothing here ever holds a whole song in
//! memory. `Connection: close` keeps the parser simple: one request, one connection.

use std::io;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

use crate::{ProviderError, Result};

/// A connection that may be plain TCP or TLS.
trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HEAD: usize = 64 * 1024;
const MAX_REDIRECTS: usize = 6;

/// A browser user agent. Some endpoints answer differently (or not at all) without one.
pub(crate) const DESKTOP_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

/// The parts of a URL this client needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Url {
    pub https: bool,
    pub host: String,
    pub port: u16,
    /// Path and query, always starting with `/`.
    pub path: String,
}

impl Url {
    pub(crate) fn parse(url: &str) -> Result<Url> {
        let (https, rest) = match url.strip_prefix("https://") {
            Some(r) => (true, r),
            None => match url.strip_prefix("http://") {
                Some(r) => (false, r),
                None => return Err(ProviderError::Invalid(format!("not an http(s) link: {url}"))),
            },
        };
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..end];
        if authority.is_empty() || authority.contains('@') || authority.contains('[') {
            return Err(ProviderError::Invalid(format!("bad link authority: {authority}")));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
                (h, p.parse::<u16>().map_err(|_| ProviderError::Invalid(format!("bad port: {p}")))?)
            }
            _ => (authority, if https { 443 } else { 80 }),
        };
        if host.is_empty() || host.len() > 255 || host.chars().any(|c| c.is_control() || c.is_whitespace() || !c.is_ascii()) {
            return Err(ProviderError::Invalid(format!("bad host: {host}")));
        }
        let mut path = rest[end..].to_string();
        if let Some(hash) = path.find('#') {
            path.truncate(hash);
        }
        if path.is_empty() {
            path.push('/');
        } else if path.starts_with('?') {
            path.insert(0, '/');
        }
        Ok(Url { https, host: host.to_ascii_lowercase(), port, path })
    }

    fn authority(&self) -> String {
        let default = if self.https { 443 } else { 80 };
        if self.port == default {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    fn origin(&self) -> String {
        format!("{}://{}", if self.https { "https" } else { "http" }, self.authority())
    }
}

/// Resolves a `Location` header against the URL that produced it.
pub(crate) fn resolve_location(base: &Url, location: &str) -> Result<String> {
    let location = location.trim();
    if location.is_empty() || location.len() > 4096 || location.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(ProviderError::Tool("the server sent an unusable redirect".into()));
    }
    if location.starts_with("https://") || location.starts_with("http://") {
        return Ok(location.to_string());
    }
    if let Some(rest) = location.strip_prefix("//") {
        return Ok(format!("{}://{rest}", if base.https { "https" } else { "http" }));
    }
    if location.starts_with('/') {
        return Ok(format!("{}{}", base.origin(), normalize_path(location)));
    }
    // Relative to the directory of the current path.
    let dir = match base.path.rfind('/') {
        Some(i) => &base.path[..=i],
        None => "/",
    };
    Ok(format!("{}{}", base.origin(), normalize_path(&format!("{dir}{location}"))))
}

/// Removes `.` and `..` segments, so a relative audio segment cannot climb out of its folder.
fn normalize_path(path: &str) -> String {
    let (path, query) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path, None),
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let mut out = String::from("/");
    out.push_str(&parts.join("/"));
    if let Some(query) = query {
        out.push('?');
        out.push_str(query);
    }
    out
}

/// One HTTP response, with its body still arriving.
pub(crate) struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    body: Body,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The stated body length, when the server sent one.
    pub fn content_length(&self) -> Option<u64> {
        self.header("content-length").and_then(|v| v.trim().parse().ok())
    }

    /// Reads the next piece of the body. `Ok(0)` means the body ended.
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.body.read(buf).await
    }

    /// Reads the whole body, refusing anything over `cap` bytes.
    pub async fn bytes(&mut self, cap: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(8 * 1024);
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = self.read(&mut buf).await?;
            if n == 0 {
                return Ok(out);
            }
            if out.len() + n > cap {
                return Err(ProviderError::Parse(format!("the answer is larger than {cap} bytes")));
            }
            out.extend_from_slice(&buf[..n]);
        }
    }
}

/// The request that produced a response, for errors.
pub(crate) struct Request<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub headers: Vec<(String, String)>,
    pub body: Option<&'a [u8]>,
}

impl<'a> Request<'a> {
    pub fn get(url: &'a str) -> Request<'a> {
        Request { method: "GET", url, headers: Vec::new(), body: None }
    }

    pub fn post(url: &'a str, body: &'a [u8]) -> Request<'a> {
        Request { method: "POST", url, headers: Vec::new(), body: Some(body) }
    }

    pub fn header(mut self, name: &str, value: &str) -> Request<'a> {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn range(self, offset: u64, len: u64) -> Request<'a> {
        let last = offset.saturating_add(len).saturating_sub(1);
        self.header("Range", &format!("bytes={offset}-{last}"))
    }
}

/// Sends `request` and follows redirects. The body is not read yet.
pub(crate) async fn send(request: Request<'_>) -> Result<Response> {
    let mut url = Url::parse(request.url)?;
    let method = request.method;
    let body = request.body;
    for _ in 0..=MAX_REDIRECTS {
        let mut headers = request.headers.clone();
        let mut stream = connect(&url).await?;
        let head = build_head(method, &url, &headers, body);
        if tokio::time::timeout(HEAD_TIMEOUT, stream.write_all(head.as_bytes())).await.is_err() {
            return Err(ProviderError::Tool("the request timed out".into()));
        }
        if let Some(body) = body {
            if tokio::time::timeout(HEAD_TIMEOUT, stream.write_all(body)).await.is_err() {
                return Err(ProviderError::Tool("the request timed out".into()));
            }
        }
        let (status, reply, leftover) = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut stream)).await {
            Ok(r) => r?,
            Err(_) => return Err(ProviderError::Tool("the server took too long to answer".into())),
        };
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let Some(location) = reply.iter().find(|(k, _)| k.eq_ignore_ascii_case("location")).map(|(_, v)| v.clone()) else {
                return Err(ProviderError::Tool(format!("the server redirected without a location ({status})")));
            };
            let next = resolve_location(&url, &location)?;
            url = Url::parse(&next)?;
            headers.clear();
            continue;
        }
        let mode = body_mode(&reply);
        return Ok(Response { status, headers: reply, body: Body::new(stream, mode, leftover) });
    }
    Err(ProviderError::Tool("too many redirects".into()))
}

/// GET with extra headers, following redirects.
pub(crate) async fn get(url: &str, headers: &[(&str, &str)]) -> Result<Response> {
    let mut r = Request::get(url);
    for (k, v) in headers {
        r = r.header(k, v);
    }
    send(r).await
}

/// GET with a byte range.
pub(crate) async fn get_range(url: &str, offset: u64, len: u64, headers: &[(&str, &str)]) -> Result<Response> {
    let mut r = Request::get(url).range(offset, len);
    for (k, v) in headers {
        r = r.header(k, v);
    }
    send(r).await
}

/// POST a JSON body, following redirects.
pub(crate) async fn post_json(url: &str, body: &[u8], headers: &[(&str, &str)]) -> Result<Response> {
    let mut r = Request::post(url, body).header("Content-Type", "application/json");
    for (k, v) in headers {
        r = r.header(k, v);
    }
    send(r).await
}

/// GETs a JSON document and fails on a non-200 answer. `what` names the service in errors.
pub(crate) async fn get_json(url: &str, headers: &[(&str, &str)], cap: usize, what: &str) -> Result<serde_json::Value> {
    let mut r = get(url, headers).await?;
    let status = r.status;
    let bytes = r.bytes(cap).await?;
    if status != 200 {
        return Err(status_error(what, status, &bytes));
    }
    serde_json::from_slice(&bytes).map_err(|e| ProviderError::Parse(format!("{what} sent an answer this app cannot read: {e}")))
}

/// POSTs JSON and returns a 200 answer as JSON.
pub(crate) async fn post_json_value(url: &str, body: &[u8], headers: &[(&str, &str)], cap: usize, what: &str) -> Result<serde_json::Value> {
    let mut r = post_json(url, body, headers).await?;
    let status = r.status;
    let bytes = r.bytes(cap).await?;
    if status != 200 {
        return Err(status_error(what, status, &bytes));
    }
    serde_json::from_slice(&bytes).map_err(|e| ProviderError::Parse(format!("{what} sent an answer this app cannot read: {e}")))
}

/// A message a person can act on, with the service's own wording when there is some.
pub(crate) fn status_error(what: &str, status: u16, body: &[u8]) -> ProviderError {
    let detail = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.get("error"))
                .or_else(|| v.get("message"))
                .and_then(|m| m.as_str().map(str::to_string))
        })
        .filter(|m| !m.is_empty() && m.len() < 300);
    match (status, detail) {
        (401 | 403, Some(m)) => ProviderError::Tool(format!("{what} refused the request: {m}")),
        (401 | 403, None) => ProviderError::Tool(format!("{what} refused the request ({status})")),
        (404, _) => ProviderError::Tool(format!("{what} does not have that item")),
        (429, _) => ProviderError::Tool(format!("{what} is rate limiting this app; try again in a minute")),
        (_, Some(m)) => ProviderError::Tool(format!("{what} answered {status}: {m}")),
        (_, None) => ProviderError::Tool(format!("{what} answered {status}")),
    }
}

fn build_head(method: &str, url: &Url, headers: &[(String, String)], body: Option<&[u8]>) -> String {
    let mut head = format!("{method} {} HTTP/1.1\r\nHost: {}\r\n", url.path, url.authority());
    head.push_str("Accept-Encoding: identity\r\nConnection: close\r\n");
    if body.is_some() {
        head.push_str(&format!("Content-Length: {}\r\n", body.map(|b| b.len()).unwrap_or(0)));
    }
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("user-agent")) {
        head.push_str("User-Agent: ");
        head.push_str(DESKTOP_UA);
        head.push_str("\r\n");
    }
    for (k, v) in headers {
        // A header value with a line break could split the request; drop those.
        if k.chars().any(|c| c.is_control() || c == ':') || v.chars().any(|c| c.is_control()) {
            continue;
        }
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    head
}

/// Reads the status line and headers, and returns the bytes that already belong to the body.
async fn read_head(stream: &mut Box<dyn Stream>) -> Result<(u16, Vec<(String, String)>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(4096);
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await.map_err(|e| ProviderError::Io(e.to_string()))?;
        if n == 0 {
            return Err(ProviderError::Tool("the server closed the connection before answering".into()));
        }
        buf.push(byte[0]);
        if buf.len() >= 4 && buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX_HEAD {
            return Err(ProviderError::Parse("the server sent oversized headers".into()));
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| ProviderError::Parse(format!("bad status line: {status_line}")))?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    Ok((status, headers, Vec::new()))
}

fn body_mode(headers: &[(String, String)]) -> Mode {
    let get = |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str());
    if get("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        return Mode::Chunked { remaining: 0, done: false };
    }
    if let Some(len) = get("content-length").and_then(|v| v.trim().parse::<u64>().ok()) {
        return Mode::Length(len);
    }
    Mode::Eof
}

enum Mode {
    Length(u64),
    Chunked { remaining: u64, done: bool },
    Eof,
}

struct Body {
    stream: Box<dyn Stream>,
    mode: Mode,
    prefix: Vec<u8>,
    prefix_at: usize,
}

impl Body {
    fn new(stream: Box<dyn Stream>, mode: Mode, prefix: Vec<u8>) -> Body {
        Body { stream, mode, prefix, prefix_at: 0 }
    }

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.prefix_at < self.prefix.len() {
            let n = (self.prefix.len() - self.prefix_at).min(buf.len());
            buf[..n].copy_from_slice(&self.prefix[self.prefix_at..self.prefix_at + n]);
            self.prefix_at += n;
            return Ok(n);
        }
        match &mut self.mode {
            Mode::Length(left) => {
                if *left == 0 {
                    return Ok(0);
                }
                let want = (*left).min(buf.len() as u64) as usize;
                let n = self.stream.read(&mut buf[..want]).await.map_err(|e| ProviderError::Io(e.to_string()))?;
                if n == 0 {
                    return Err(ProviderError::Tool("the connection ended before the file did".into()));
                }
                *left -= n as u64;
                Ok(n)
            }
            Mode::Eof => self.stream.read(buf).await.map_err(|e| ProviderError::Io(e.to_string())),
            Mode::Chunked { remaining, done } => {
                if *done {
                    return Ok(0);
                }
                if *remaining == 0 {
                    let line = read_line(&mut self.stream, 64).await?;
                    let size = parse_chunk_size(&line)
                        .ok_or_else(|| ProviderError::Parse(format!("bad chunk size: {line:?}")))?;
                    if size == 0 {
                        // Trailers, then a blank line.
                        loop {
                            let line = read_line(&mut self.stream, MAX_HEAD).await?;
                            if line.is_empty() {
                                break;
                            }
                        }
                        *done = true;
                        return Ok(0);
                    }
                    *remaining = size as u64;
                }
                let want = (*remaining).min(buf.len() as u64) as usize;
                let n = self.stream.read(&mut buf[..want]).await.map_err(|e| ProviderError::Io(e.to_string()))?;
                if n == 0 {
                    return Err(ProviderError::Tool("the connection ended inside a chunk".into()));
                }
                *remaining -= n as u64;
                if *remaining == 0 {
                    let mut crlf = [0u8; 2];
                    self.stream.read_exact(&mut crlf).await.map_err(|e| ProviderError::Io(e.to_string()))?;
                    if &crlf != b"\r\n" {
                        return Err(ProviderError::Parse("a chunk did not end with a line break".into()));
                    }
                }
                Ok(n)
            }
        }
    }
}

async fn read_line(stream: &mut Box<dyn Stream>, cap: usize) -> Result<String> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await.map_err(|e| ProviderError::Io(e.to_string()))?;
        if n == 0 {
            return Err(ProviderError::Tool("the connection ended inside the body".into()));
        }
        if byte[0] == b'\n' {
            break;
        }
        out.push(byte[0]);
        if out.len() > cap {
            return Err(ProviderError::Parse("a line in the body is too long".into()));
        }
    }
    if out.last() == Some(&b'\r') {
        out.pop();
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// `1f` or `1f;ext=1` to a number.
pub(crate) fn parse_chunk_size(line: &str) -> Option<usize> {
    let hex = line.split(';').next()?.trim();
    if hex.is_empty() || hex.len() > 16 {
        return None;
    }
    usize::from_str_radix(hex, 16).ok()
}

/// The TLS setup is shared by every request and built once.
fn tls_config() -> &'static Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("rustls default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Arc::new(config)
    })
}

/// Opens a connection to the URL, through the saved proxy when one is set.
async fn connect(url: &Url) -> Result<Box<dyn Stream>> {
    let proxy = crate::net::proxy().map(|p| Proxy::parse(&p)).transpose()?;
    match proxy {
        None => {
            let tcp = tcp_connect(&url.host, url.port).await?;
            if url.https {
                Ok(Box::new(tls_connect(tcp, &url.host).await?))
            } else {
                Ok(Box::new(tcp))
            }
        }
        Some(proxy) => {
            let mut stream: Box<dyn Stream> = {
                let tcp = tcp_connect(&proxy.host, proxy.port).await?;
                if proxy.https {
                    Box::new(tls_connect(tcp, &proxy.host).await?)
                } else {
                    Box::new(tcp)
                }
            };
            match proxy.scheme {
                ProxyScheme::Http => {
                    if url.https {
                        tunnel(&mut stream, &proxy, &url.host, url.port).await?;
                        Ok(Box::new(tls_connect_boxed(stream, &url.host).await?))
                    } else {
                        // Plain http through an http proxy uses the absolute form.
                        // The caller writes the request; mark the host so build_head can use it.
                        // A simpler route: tunnel everything.
                        tunnel(&mut stream, &proxy, &url.host, url.port).await?;
                        Ok(stream)
                    }
                }
                ProxyScheme::Socks5 => {
                    socks5(&mut stream, &proxy, &url.host, url.port).await?;
                    if url.https {
                        Ok(Box::new(tls_connect_boxed(stream, &url.host).await?))
                    } else {
                        Ok(stream)
                    }
                }
                ProxyScheme::Socks4 => {
                    socks4(&mut stream, &proxy, &url.host, url.port).await?;
                    if url.https {
                        Ok(Box::new(tls_connect_boxed(stream, &url.host).await?))
                    } else {
                        Ok(stream)
                    }
                }
            }
        }
    }
}

async fn tcp_connect(host: &str, port: u16) -> Result<TcpStream> {
    let fut = TcpStream::connect((host, port));
    match tokio::time::timeout(CONNECT_TIMEOUT, fut).await {
        Ok(Ok(s)) => {
            let _ = s.set_nodelay(true);
            Ok(s)
        }
        Ok(Err(e)) => Err(ProviderError::Io(format!("cannot reach {host}:{port}: {e}"))),
        Err(_) => Err(ProviderError::Tool(format!("connecting to {host}:{port} timed out"))),
    }
}

async fn tls_connect(tcp: TcpStream, host: &str) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let name = ServerName::try_from(host.to_string()).map_err(|_| ProviderError::Invalid(format!("bad TLS name: {host}")))?;
    let connector = TlsConnector::from(tls_config().clone());
    match tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(name, tcp)).await {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(ProviderError::Io(format!("TLS with {host} failed: {e}"))),
        Err(_) => Err(ProviderError::Tool(format!("TLS with {host} timed out"))),
    }
}

/// TLS over a boxed stream (a proxy tunnel).
async fn tls_connect_boxed(stream: Box<dyn Stream>, host: &str) -> Result<Box<dyn Stream>> {
    struct Boxed(Box<dyn Stream>);
    impl AsyncRead for Boxed {
        fn poll_read(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<io::Result<()>> {
            std::pin::Pin::new(&mut *self.0).poll_read(cx, buf)
        }
    }
    impl AsyncWrite for Boxed {
        fn poll_write(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<io::Result<usize>> {
            std::pin::Pin::new(&mut *self.0).poll_write(cx, buf)
        }
        fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<io::Result<()>> {
            std::pin::Pin::new(&mut *self.0).poll_flush(cx)
        }
        fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<io::Result<()>> {
            std::pin::Pin::new(&mut *self.0).poll_shutdown(cx)
        }
    }
    let name = ServerName::try_from(host.to_string()).map_err(|_| ProviderError::Invalid(format!("bad TLS name: {host}")))?;
    let connector = TlsConnector::from(tls_config().clone());
    match tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(name, Boxed(stream))).await {
        Ok(Ok(s)) => Ok(Box::new(s)),
        Ok(Err(e)) => Err(ProviderError::Io(format!("TLS with {host} failed: {e}"))),
        Err(_) => Err(ProviderError::Tool(format!("TLS with {host} timed out"))),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProxyScheme {
    Http,
    Socks5,
    Socks4,
}

struct Proxy {
    scheme: ProxyScheme,
    https: bool,
    host: String,
    port: u16,
    user: Option<String>,
    pass: Option<String>,
}

impl Proxy {
    fn parse(text: &str) -> Result<Proxy> {
        let (scheme, rest) = if let Some(r) = text.strip_prefix("http://") {
            (ProxyScheme::Http, r)
        } else if let Some(r) = text.strip_prefix("https://") {
            (ProxyScheme::Http, r)
        } else if let Some(r) = text.strip_prefix("socks5h://").or_else(|| text.strip_prefix("socks5://")) {
            (ProxyScheme::Socks5, r)
        } else if let Some(r) = text.strip_prefix("socks4a://").or_else(|| text.strip_prefix("socks4://")) {
            (ProxyScheme::Socks4, r)
        } else {
            return Err(ProviderError::Invalid("unsupported proxy scheme".into()));
        };
        let https = text.starts_with("https://");
        let (auth, hostport) = match rest.rsplit_once('@') {
            Some((a, h)) => (Some(a), h),
            None => (None, rest),
        };
        let (user, pass) = match auth {
            Some(a) => match a.split_once(':') {
                Some((u, p)) => (Some(percent_decode(u)), Some(percent_decode(p))),
                None => (Some(percent_decode(a)), None),
            },
            None => (None, None),
        };
        let (host, port) = hostport
            .rsplit_once(':')
            .and_then(|(h, p)| p.parse::<u16>().ok().map(|p| (h, p)))
            .ok_or_else(|| ProviderError::Invalid("the proxy needs host:port".into()))?;
        if host.is_empty() || host.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(ProviderError::Invalid("bad proxy host".into()));
        }
        Ok(Proxy { scheme, https, host: host.to_string(), port, user, pass })
    }

    fn authorization(&self) -> Option<String> {
        let user = self.user.as_deref().unwrap_or("");
        let pass = self.pass.as_deref().unwrap_or("");
        if user.is_empty() && pass.is_empty() {
            return None;
        }
        Some(format!("Basic {}", base64(format!("{user}:{pass}").as_bytes())))
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        out.push(TABLE[(b[0] >> 2) as usize] as char);
        out.push(TABLE[(((b[0] & 0x03) << 4) | (b[1] >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[(((b[1] & 0x0f) << 2) | (b[2] >> 6)) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(b[2] & 0x3f) as usize] as char } else { '=' });
    }
    out
}

/// `CONNECT host:port` through an http proxy.
async fn tunnel(stream: &mut Box<dyn Stream>, proxy: &Proxy, host: &str, port: u16) -> Result<()> {
    let mut head = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if let Some(auth) = proxy.authorization() {
        head.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.map_err(|e| ProviderError::Io(e.to_string()))?;
    let (status, _, _) = read_head(stream).await?;
    if status != 200 {
        return Err(ProviderError::Tool(format!("the proxy refused the tunnel ({status})")));
    }
    Ok(())
}

/// SOCKS5, with the host name sent to the proxy (no local DNS).
async fn socks5(stream: &mut Box<dyn Stream>, proxy: &Proxy, host: &str, port: u16) -> Result<()> {
    let auth = proxy.user.is_some() || proxy.pass.is_some();
    let greeting: &[u8] = if auth { &[5, 2, 0, 2] } else { &[5, 1, 0] };
    stream.write_all(greeting).await.map_err(|e| ProviderError::Io(e.to_string()))?;
    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).await.map_err(|e| ProviderError::Io(e.to_string()))?;
    if reply[0] != 5 {
        return Err(ProviderError::Tool("the proxy is not SOCKS5".into()));
    }
    if reply[1] == 2 {
        let user = proxy.user.clone().unwrap_or_default();
        let pass = proxy.pass.clone().unwrap_or_default();
        if user.len() > 255 || pass.len() > 255 {
            return Err(ProviderError::Invalid("the proxy user name or password is too long".into()));
        }
        let mut msg = vec![1, user.len() as u8];
        msg.extend_from_slice(user.as_bytes());
        msg.push(pass.len() as u8);
        msg.extend_from_slice(pass.as_bytes());
        stream.write_all(&msg).await.map_err(|e| ProviderError::Io(e.to_string()))?;
        stream.read_exact(&mut reply).await.map_err(|e| ProviderError::Io(e.to_string()))?;
        if reply[1] != 0 {
            return Err(ProviderError::Tool("the proxy rejected the user name or password".into()));
        }
    } else if reply[1] != 0 {
        return Err(ProviderError::Tool("the proxy wants an authentication method this app does not use".into()));
    }
    if host.len() > 255 {
        return Err(ProviderError::Invalid("bad proxy target".into()));
    }
    let mut msg = vec![5, 1, 0, 3, host.len() as u8];
    msg.extend_from_slice(host.as_bytes());
    msg.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&msg).await.map_err(|e| ProviderError::Io(e.to_string()))?;
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await.map_err(|e| ProviderError::Io(e.to_string()))?;
    if head[1] != 0 {
        return Err(ProviderError::Tool(format!("the proxy could not reach {host} ({})", head[1])));
    }
    let skip = match head[3] {
        1 => 4,
        3 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await.map_err(|e| ProviderError::Io(e.to_string()))?;
            len[0] as usize
        }
        4 => 16,
        _ => return Err(ProviderError::Tool("the proxy sent a bad address".into())),
    };
    let mut ignore = vec![0u8; skip + 2];
    stream.read_exact(&mut ignore).await.map_err(|e| ProviderError::Io(e.to_string()))?;
    Ok(())
}

/// SOCKS4a: the host name goes to the proxy after the (empty) user id.
async fn socks4(stream: &mut Box<dyn Stream>, proxy: &Proxy, host: &str, port: u16) -> Result<()> {
    let user = proxy.user.clone().unwrap_or_default();
    let mut msg = vec![4, 1];
    msg.extend_from_slice(&port.to_be_bytes());
    msg.extend_from_slice(&[0, 0, 0, 1]);
    msg.extend_from_slice(user.as_bytes());
    msg.push(0);
    msg.extend_from_slice(host.as_bytes());
    msg.push(0);
    stream.write_all(&msg).await.map_err(|e| ProviderError::Io(e.to_string()))?;
    let mut reply = [0u8; 8];
    stream.read_exact(&mut reply).await.map_err(|e| ProviderError::Io(e.to_string()))?;
    if reply[1] != 0x5a {
        return Err(ProviderError::Tool(format!("the proxy could not reach {host} ({})", reply[1])));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let u = Url::parse("https://www.youtube.com/youtubei/v1/player?prettyPrint=false").unwrap();
        assert!(u.https);
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("www.youtube.com", 443, "/youtubei/v1/player?prettyPrint=false"));
        assert_eq!(u.origin(), "https://www.youtube.com");
        let u = Url::parse("http://127.0.0.1:8080").unwrap();
        assert_eq!((u.https, u.host.as_str(), u.port, u.path.as_str()), (false, "127.0.0.1", 8080, "/"));
        assert_eq!(u.authority(), "127.0.0.1:8080");
        assert_eq!(Url::parse("https://h/p#frag").unwrap().path, "/p");
        assert_eq!(Url::parse("https://h?x=1").unwrap().path, "/?x=1");
    }

    #[test]
    fn refuses_bad_urls() {
        for bad in ["ftp://x/", "https://", "https://u:p@h/", "https://h:99999/", "https://h a/"] {
            assert!(Url::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn resolves_redirects() {
        let base = Url::parse("https://api.example.com/v1/tracks/1/stream?x=1").unwrap();
        assert_eq!(resolve_location(&base, "https://cdn.example.com/a").unwrap(), "https://cdn.example.com/a");
        assert_eq!(resolve_location(&base, "//cdn.example.com/a").unwrap(), "https://cdn.example.com/a");
        assert_eq!(resolve_location(&base, "/b").unwrap(), "https://api.example.com/b");
        assert_eq!(resolve_location(&base, "b").unwrap(), "https://api.example.com/v1/tracks/1/b");
        assert_eq!(resolve_location(&base, "../b").unwrap(), "https://api.example.com/v1/tracks/b");
        assert_eq!(resolve_location(&base, "/a/../b?x=1").unwrap(), "https://api.example.com/b?x=1");
        assert!(resolve_location(&base, "https://x/\nInjected: 1").is_err());
    }

    #[test]
    fn reads_chunk_sizes() {
        assert_eq!(parse_chunk_size("1f"), Some(31));
        assert_eq!(parse_chunk_size("0"), Some(0));
        assert_eq!(parse_chunk_size("A;ext=1"), Some(10));
        assert_eq!(parse_chunk_size(""), None);
        assert_eq!(parse_chunk_size("nope"), None);
        assert_eq!(parse_chunk_size("00000000000000000001"), None);
    }

    #[test]
    fn builds_a_head_without_header_injection() {
        let url = Url::parse("https://h/p").unwrap();
        let headers = vec![("X-Ok".to_string(), "1".to_string()), ("X-Bad".to_string(), "a\r\nEvil: 1".to_string())];
        let head = build_head("GET", &url, &headers, None);
        assert!(head.starts_with("GET /p HTTP/1.1\r\nHost: h\r\n"));
        assert!(head.contains("X-Ok: 1\r\n"));
        assert!(!head.contains("Evil"));
        assert!(head.ends_with("\r\n\r\n"));
    }

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
    }
}
