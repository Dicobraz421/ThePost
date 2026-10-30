//! Минимальный клиент HTTP/1.1 поверх TLS для API хранилища секретов (PLT-12).
//!
//! Одно соединение на запрос (`Connection: close`): секреты читаются при запуске и перечитывании конфигурации,
//! пул соединений не нужен. Тело ответа — по `Content-Length`, `Transfer-Encoding: chunked` или до закрытия
//! соединения; размер ответа ограничен, таймаут — на весь обмен. Открытый HTTP — только по явному выбору
//! вызывающего (профиль dev).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

/// Предел размера заголовков ответа, байт.
const MAX_HEAD: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum HttpError {
    #[error("url: {0}")]
    Url(&'static str),
    #[error("request: {0}")]
    Request(&'static str),
    #[error("connect {addr}: {source}")]
    Connect {
        addr: String,
        source: std::io::Error,
    },
    #[error("tls handshake: {0}")]
    Tls(std::io::Error),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("no response within {0:?}")]
    Timeout(Duration),
    #[error("malformed response: {0}")]
    Malformed(&'static str),
    #[error("response larger than {0} bytes")]
    TooLarge(usize),
}

/// Адрес сервера из URL `https://host[:port]` (`http://` — открытый канал).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    /// Канал TLS.
    pub(crate) tls: bool,
    /// Имя или адрес узла (IPv6 — без скобок).
    pub(crate) host: String,
    pub(crate) port: u16,
}

impl Endpoint {
    /// Разбирает адрес; путь, параметры и учётные данные в адресе не допускаются.
    pub(crate) fn parse(url: &str) -> Result<Self, HttpError> {
        let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(HttpError::Url("expected https://host[:port]"));
        };
        let authority = match rest.split_once('/') {
            Some((authority, "")) => authority,
            Some(_) => {
                return Err(HttpError::Url(
                    "path is not allowed, expected https://host[:port]",
                ));
            }
            None => rest,
        };
        if authority.contains('@') {
            return Err(HttpError::Url("credentials in the address are not allowed"));
        }
        if authority.contains(['?', '#']) || authority.chars().any(char::is_whitespace) {
            return Err(HttpError::Url("expected https://host[:port]"));
        }
        let default_port = if tls { 443 } else { 80 };
        let port = |p: &str| p.parse::<u16>().map_err(|_| HttpError::Url("invalid port"));
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (host, tail) = v6
                .split_once(']')
                .ok_or(HttpError::Url("unclosed [ in IPv6 address"))?;
            let port = match tail {
                "" => default_port,
                tail => port(
                    tail.strip_prefix(':')
                        .ok_or(HttpError::Url("invalid port"))?,
                )?,
            };
            (host, port)
        } else {
            match authority.rsplit_once(':') {
                Some((host, p)) => (host, port(p)?),
                None => (authority, default_port),
            }
        };
        if host.is_empty() {
            return Err(HttpError::Url("host is missing"));
        }
        Ok(Self {
            tls,
            host: host.to_owned(),
            port,
        })
    }

    /// Значение заголовка `Host`.
    fn host_header(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = if self.tls { "https" } else { "http" };
        write!(f, "{scheme}://{}", self.host_header())
    }
}

/// Ответ сервера.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Response {
    pub(crate) status: u16,
    /// Заголовки: имя в нижнем регистре и значение.
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Response {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Клиент одного сервера.
#[derive(Clone)]
pub(crate) struct HttpClient {
    endpoint: Endpoint,
    /// `None` — открытый канал (адрес `http://`).
    tls: Option<TlsConnector>,
    timeout: Duration,
    max_body: usize,
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpClient")
            .field("endpoint", &self.endpoint.to_string())
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl HttpClient {
    /// Клиент `endpoint`; для `https://` нужна конфигурация TLS `tls`.
    pub(crate) fn new(
        endpoint: Endpoint,
        tls: Option<Arc<ClientConfig>>,
        timeout: Duration,
        max_body: usize,
    ) -> Result<Self, HttpError> {
        let tls = match (endpoint.tls, tls) {
            (true, Some(cfg)) => Some(TlsConnector::from(cfg)),
            (true, None) => return Err(HttpError::Url("https requires a TLS configuration")),
            (false, _) => None,
        };
        Ok(Self {
            endpoint,
            tls,
            timeout,
            max_body,
        })
    }

    pub(crate) fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Выполняет запрос `method path`; тело `body` — с `Content-Type: application/json`.
    pub(crate) async fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<Response, HttpError> {
        let head = self.head(method, path, headers, body)?;
        tokio::time::timeout(self.timeout, self.exchange(head.as_bytes(), body))
            .await
            .map_err(|_| HttpError::Timeout(self.timeout))?
    }

    fn head(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<String, HttpError> {
        let bad = |s: &str| s.is_empty() || s.bytes().any(|b| b.is_ascii_control() || b == b' ');
        if bad(method) || bad(path) || !path.starts_with('/') {
            return Err(HttpError::Request("invalid method or path"));
        }
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: application/json\r\nUser-Agent: thepost\r\n",
            self.endpoint.host_header()
        );
        for (name, value) in headers {
            if bad(name) || value.bytes().any(|b| b == b'\r' || b == b'\n') {
                return Err(HttpError::Request("invalid header"));
            }
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        if let Some(body) = body {
            head.push_str("Content-Type: application/json\r\n");
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");
        Ok(head)
    }

    async fn exchange(&self, head: &[u8], body: Option<&[u8]>) -> Result<Response, HttpError> {
        let addr = (self.endpoint.host.as_str(), self.endpoint.port);
        let tcp = TcpStream::connect(addr)
            .await
            .map_err(|source| HttpError::Connect {
                addr: self.endpoint.host_header(),
                source,
            })?;
        let _ = tcp.set_nodelay(true);
        match &self.tls {
            Some(connector) => {
                let name = ServerName::try_from(self.endpoint.host.clone())
                    .map_err(|_| HttpError::Url("invalid host name"))?;
                let mut tls = connector.connect(name, tcp).await.map_err(HttpError::Tls)?;
                roundtrip(&mut tls, head, body, self.max_body).await
            }
            None => {
                let mut tcp = tcp;
                roundtrip(&mut tcp, head, body, self.max_body).await
            }
        }
    }
}

async fn roundtrip<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    head: &[u8],
    body: Option<&[u8]>,
    max_body: usize,
) -> Result<Response, HttpError> {
    stream.write_all(head).await?;
    if let Some(body) = body {
        stream.write_all(body).await?;
    }
    stream.flush().await?;
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 16 * 1024];
    loop {
        let n = match stream.read(&mut chunk).await {
            Ok(n) => n,
            // Сервер закрыл TCP без close_notify: ответ разбирается по прочитанному.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
            Err(e) => return Err(e.into()),
        };
        buf.extend_from_slice(&chunk[..n]);
        if let Some(resp) = parse(&buf, n == 0, max_body)? {
            return Ok(resp);
        }
        if n == 0 {
            return Err(HttpError::Malformed(
                "connection closed before the response ended",
            ));
        }
    }
}

/// Разбирает ответ из `buf`; `None` — ответ ещё не дочитан. `eof` — соединение закрыто.
pub(crate) fn parse(buf: &[u8], eof: bool, max_body: usize) -> Result<Option<Response>, HttpError> {
    let Some(head_end) = find(buf, b"\r\n\r\n") else {
        if buf.len() > MAX_HEAD {
            return Err(HttpError::TooLarge(MAX_HEAD));
        }
        return if eof {
            Err(HttpError::Malformed("no response headers"))
        } else {
            Ok(None)
        };
    };
    let head = std::str::from_utf8(&buf[..head_end])
        .map_err(|_| HttpError::Malformed("headers are not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let (version, code) = (parts.next(), parts.next());
    if !version.is_some_and(|v| v.starts_with("HTTP/1.")) {
        return Err(HttpError::Malformed("status line"));
    }
    let status = code
        .and_then(|c| c.parse::<u16>().ok())
        .filter(|c| (100..600).contains(c))
        .ok_or(HttpError::Malformed("status code"))?;
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or(HttpError::Malformed("header line"))?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
    let rest = &buf[head_end + 4..];
    let header = |n: &str| {
        headers
            .iter()
            .find(|(h, _)| h == n)
            .map(|(_, v)| v.as_str())
    };
    let chunked = header("transfer-encoding").is_some_and(|te| {
        te.rsplit(',')
            .next()
            .is_some_and(|last| last.trim().eq_ignore_ascii_case("chunked"))
    });
    let body = if status == 204 || status == 304 || (100..200).contains(&status) {
        Some(Vec::new())
    } else if chunked {
        dechunk(rest, max_body)?
    } else if let Some(len) = header("content-length") {
        let len: usize = len
            .parse()
            .map_err(|_| HttpError::Malformed("content-length"))?;
        if len > max_body {
            return Err(HttpError::TooLarge(max_body));
        }
        if rest.len() >= len {
            Some(rest[..len].to_vec())
        } else if eof {
            return Err(HttpError::Malformed("body shorter than content-length"));
        } else {
            None
        }
    } else if eof {
        Some(rest.to_vec())
    } else {
        None
    };
    if rest.len() > max_body.saturating_add(MAX_HEAD) {
        return Err(HttpError::TooLarge(max_body));
    }
    match body {
        Some(body) if body.len() > max_body => Err(HttpError::TooLarge(max_body)),
        Some(body) => Ok(Some(Response {
            status,
            headers,
            body,
        })),
        None if eof => Err(HttpError::Malformed("truncated body")),
        None => Ok(None),
    }
}

/// Тело `Transfer-Encoding: chunked`; `None` — не дочитано.
fn dechunk(mut data: &[u8], max_body: usize) -> Result<Option<Vec<u8>>, HttpError> {
    let mut out = Vec::new();
    loop {
        let Some(line_end) = find(data, b"\r\n") else {
            return Ok(None);
        };
        let line = std::str::from_utf8(&data[..line_end])
            .map_err(|_| HttpError::Malformed("chunk size"))?;
        let size = line.split(';').next().unwrap_or_default().trim();
        let size =
            usize::from_str_radix(size, 16).map_err(|_| HttpError::Malformed("chunk size"))?;
        data = &data[line_end + 2..];
        if size == 0 {
            // Завершающие поля (trailers) не нужны: ответ закончен пустой строкой.
            return Ok(find(data, b"\r\n").map(|_| out));
        }
        if out.len().saturating_add(size) > max_body {
            return Err(HttpError::TooLarge(max_body));
        }
        if data.len() < size + 2 {
            return Ok(None);
        }
        if &data[size..size + 2] != b"\r\n" {
            return Err(HttpError::Malformed("chunk terminator"));
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parse() {
        let e = Endpoint::parse("https://openbao.example.ru:8200").expect("https");
        assert_eq!(
            (e.tls, e.host.as_str(), e.port),
            (true, "openbao.example.ru", 8200)
        );
        assert_eq!(e.to_string(), "https://openbao.example.ru:8200");
        let e = Endpoint::parse("https://vault/").expect("slash");
        assert_eq!((e.host.as_str(), e.port), ("vault", 443));
        let e = Endpoint::parse("http://[::1]:8200").expect("v6");
        assert_eq!((e.tls, e.host.as_str(), e.port), (false, "::1", 8200));
        assert_eq!(e.host_header(), "[::1]:8200");
        assert_eq!(Endpoint::parse("https://[::1]").expect("v6").port, 443);
        for bad in [
            "openbao:8200",
            "ftp://x",
            "https://",
            "https://h:port",
            "https://h:70000",
            "https://u:p@h:8200",
            "https://h/v1",
            "https://h?x=1",
            "https://[::1",
            "https://[::1]x",
        ] {
            assert!(Endpoint::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn parse_content_length_and_partial() {
        let full = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-A: b\r\n\r\nhello";
        let r = parse(full, false, 100).expect("ok").expect("complete");
        assert_eq!((r.status, r.body.as_slice()), (200, &b"hello"[..]));
        assert_eq!(r.header("x-a"), Some("b"));
        assert_eq!(
            parse(&full[..full.len() - 1], false, 100).expect("ok"),
            None
        );
        assert!(parse(&full[..full.len() - 1], true, 100).is_err());
        assert!(parse(&full[..10], false, 100).expect("ok").is_none());
        assert!(parse(&full[..10], true, 100).is_err());
        assert!(matches!(parse(full, false, 4), Err(HttpError::TooLarge(4))));
    }

    #[test]
    fn parse_chunked_and_eof() {
        let chunked = b"HTTP/1.1 404 Not Found\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"er\r\n5;x=y\r\nrors\"\r\n1\r\n}\r\n0\r\n\r\n";
        let r = parse(chunked, false, 100).expect("ok").expect("complete");
        assert_eq!((r.status, r.body.as_slice()), (404, &br#"{"errors"}"#[..]));
        assert_eq!(
            parse(&chunked[..chunked.len() - 2], false, 100).expect("ok"),
            None
        );
        assert!(matches!(
            parse(chunked, false, 5),
            Err(HttpError::TooLarge(5))
        ));
        let bad = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n";
        assert!(parse(bad, false, 100).is_err());

        let close = b"HTTP/1.0 503 Service Unavailable\r\n\r\nsealed";
        assert_eq!(parse(close, false, 100).expect("ok"), None);
        let r = parse(close, true, 100).expect("ok").expect("eof");
        assert_eq!((r.status, r.body.as_slice()), (503, &b"sealed"[..]));

        let empty = b"HTTP/1.1 204 No Content\r\n\r\n";
        assert_eq!(
            parse(empty, false, 100).expect("ok").expect("204").body,
            b""
        );
        assert!(parse(b"SMTP 220 hi\r\n\r\n", true, 100).is_err());
        assert!(parse(b"HTTP/1.1 999 X\r\n\r\n", true, 100).is_err());
    }

    #[test]
    fn request_head_rejects_injection() {
        let e = Endpoint::parse("http://127.0.0.1:1").expect("endpoint");
        let c = HttpClient::new(e, None, Duration::from_secs(1), 1024).expect("client");
        let head = c
            .head("POST", "/v1/x", &[("X-Vault-Token", "t")], Some(b"{}"))
            .expect("head");
        assert!(
            head.starts_with("POST /v1/x HTTP/1.1\r\nHost: 127.0.0.1:1\r\n"),
            "{head}"
        );
        assert!(
            head.contains("X-Vault-Token: t\r\n") && head.ends_with("Content-Length: 2\r\n\r\n")
        );
        assert!(c.head("GET", "/v1/x y", &[], None).is_err());
        assert!(c.head("GET", "v1", &[], None).is_err());
        assert!(c.head("GET", "/v1", &[("X", "a\r\nB: c")], None).is_err());
        assert!(c.head("GET", "/v1/x\r\n", &[], None).is_err());
        assert!(
            HttpClient::new(
                Endpoint::parse("https://h").expect("e"),
                None,
                Duration::from_secs(1),
                1
            )
            .is_err()
        );
    }
}
