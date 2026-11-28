//! Minimal HTTP/1.1 helpers for the forwarder (head parsing with `httparse`, synthetic
//! JSON responses, a bounded chunked decoder). Bodies are never parsed except for the
//! sanitized `GET /slots`.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt};

/// Largest request / response head accepted (bytes up to and including `\r\n\r\n`).
pub const MAX_HEAD: usize = 32 * 1024;
const MAX_HEADERS: usize = 96;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    pub method: String,
    pub target: String,
    /// Header names lower-cased, values as sent (must be UTF-8).
    pub headers: Vec<(String, String)>,
}

impl RequestHead {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn header_count(&self, name: &str) -> usize {
        self.headers.iter().filter(|(k, _)| k == name).count()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

impl ResponseHead {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn is_chunked(&self) -> bool {
        self.header("transfer-encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    }

    pub fn is_event_stream(&self) -> bool {
        self.header("content-type")
            .is_some_and(|v| v.to_ascii_lowercase().starts_with("text/event-stream"))
    }
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn header_pairs(headers: &[httparse::Header<'_>]) -> io::Result<Vec<(String, String)>> {
    headers
        .iter()
        .map(|h| {
            let value = std::str::from_utf8(h.value).map_err(|_| bad("non-UTF-8 header value"))?;
            Ok((h.name.to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect()
}

/// Parse a complete request head. Only HTTP/1.1 and origin-form targets (`/…`).
pub fn parse_request_head(buf: &[u8]) -> io::Result<RequestHead> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(buf).map_err(|e| bad(&e.to_string()))? {
        httparse::Status::Complete(_) => {}
        httparse::Status::Partial => return Err(bad("incomplete request head")),
    }
    if req.version != Some(1) {
        return Err(bad("only HTTP/1.1 is supported"));
    }
    let target = req.path.unwrap_or("");
    if !target.starts_with('/') {
        return Err(bad("only origin-form request targets are supported"));
    }
    Ok(RequestHead {
        method: req.method.unwrap_or("").to_string(),
        target: target.to_string(),
        headers: header_pairs(req.headers)?,
    })
}

pub fn parse_response_head(buf: &[u8]) -> io::Result<ResponseHead> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut resp = httparse::Response::new(&mut headers);
    match resp.parse(buf).map_err(|e| bad(&e.to_string()))? {
        httparse::Status::Complete(_) => {}
        httparse::Status::Partial => return Err(bad("incomplete response head")),
    }
    Ok(ResponseHead {
        status: resp.code.unwrap_or(0),
        headers: header_pairs(resp.headers)?,
    })
}

/// Position just past `\r\n\r\n`, if present.
pub fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// Read until the end of an HTTP head. Returns `(head, already-read body bytes)`;
/// `Ok(None)` if the peer closed before sending anything.
pub async fn read_head<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let n = r.read(&mut chunk).await?;
        if n == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed inside the HTTP head",
            ));
        }
        let scan = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = find_head_end(&buf[scan..]).map(|e| e + scan) {
            if end > MAX_HEAD {
                return Err(bad("HTTP head too large"));
            }
            let rest = buf.split_off(end);
            return Ok(Some((buf, rest)));
        }
        if buf.len() > MAX_HEAD {
            return Err(bad("HTTP head too large"));
        }
    }
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    }
}

/// A complete `Connection: close` JSON response.
pub fn json_response(status: u16, extra_headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\n",
        reason(status),
        body.len()
    );
    for (k, v) in extra_headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("Connection: close\r\n\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// OpenAI-style error body `{"error":{"code","message","type"}}` (+ Slipstream code).
pub fn error_body(status: u16, kind: &str, slipstream_code: &str, message: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "error": {
            "code": status,
            "message": message,
            "type": kind,
            "slipstream_code": slipstream_code,
        }
    }))
    .expect("static json")
}

pub fn error_response(status: u16, slipstream_code: &str, message: &str) -> Vec<u8> {
    let kind = match status {
        400 | 408 | 413 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        503 => "unavailable_error",
        _ => "server_error",
    };
    let extra: Vec<(&str, String)> = if status == 503 {
        vec![("Retry-After", "5".to_string())]
    } else {
        Vec::new()
    };
    json_response(status, &extra, &error_body(status, kind, slipstream_code, message))
}

/// The one refusal message for everything outside the allowlist.
pub const NOT_AVAILABLE: &str = "Not available through a shared Mac";

pub fn forbidden() -> Vec<u8> {
    error_response(403, "not_allowed", NOT_AVAILABLE)
}

/// Missing / wrong local bearer token (use side with `--token-file`).
pub fn unauthorized() -> Vec<u8> {
    json_response(
        401,
        &[("WWW-Authenticate", "Bearer".to_string())],
        &error_body(
            401,
            "authentication_error",
            "bad_local_token",
            "Missing or wrong API key for the local Slipstream forwarder (use the key the Slipstream app shows).",
        ),
    )
}

/// Decode a complete chunked body (bounded; trailers ignored). `None` if malformed.
pub fn dechunk_strict(mut body: &[u8], limit: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n")?;
        let size_str = std::str::from_utf8(&body[..line_end]).ok()?;
        let size = usize::from_str_radix(size_str.split(';').next()?.trim(), 16).ok()?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        if body.len() < size + 2 || out.len() + size > limit {
            return None;
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_and_rejects_absolute_or_http10() {
        let h = parse_request_head(
            b"POST /v1/chat/completions?x=1 HTTP/1.1\r\nHost: 127.0.0.1:8080\r\nContent-Length: 2\r\n\r\n",
        )
        .unwrap();
        assert_eq!(h.method, "POST");
        assert_eq!(h.target, "/v1/chat/completions?x=1");
        assert_eq!(h.header("host"), Some("127.0.0.1:8080"));
        assert!(parse_request_head(b"GET http://evil/ HTTP/1.1\r\nHost: x\r\n\r\n").is_err());
        assert!(parse_request_head(b"GET / HTTP/1.0\r\n\r\n").is_err());
        assert!(parse_request_head(b"GET / HTTP/1.1\r\nHost: \xff\r\n\r\n").is_err());
    }

    #[test]
    fn dechunk_bounds() {
        assert_eq!(dechunk_strict(b"3\r\nabc\r\n0\r\n\r\n", 10).unwrap(), b"abc");
        assert!(dechunk_strict(b"3\r\nabc\r\n0\r\n\r\n", 2).is_none());
        assert!(dechunk_strict(b"5\r\nabc", 10).is_none());
        assert!(dechunk_strict(b"zz\r\n", 10).is_none());
    }
}
