//! Deterministic mock of llama-server's HTTP surface for the M1 forwarder tests and
//! `bench/forward/run_m1.sh` (plan §7: SSE delays, tool-call stream, disconnect
//! timestamps).
//!
//! - `POST /v1/chat/completions[?n=&gap_us=&prefill_ms=]`: chunked SSE like llama-server
//!   (the same parameters can be sent as `X-Mock-Params: n=..&gap_us=..`, which overrides
//!   the query — the share side drops query strings since the M1 security fixes, headers
//!   pass; `delay_ms=` delays any non-streamed answer)
//!   (one chunk + flush per event). Variant from the JSON body: `tool_choice` object →
//!   forced tool-call stream; `response_format` → strict-JSON content; else plain text.
//!   `"stream": false` → one JSON object with Content-Length. The head is sent with the
//!   first event (after `prefill_ms`), as llama-server does.
//! - `GET /v1/models`, `/health`, `/props`, `/metrics`, `/slots` (contains a secret
//!   prompt that must never leave the share side), `POST /tokenize|/detokenize|/apply-template`.
//! - `POST /props`, `POST /slots/*` answer 200 (as a real server with --slot-save-path
//!   would) so tests can prove the forwarder never lets them through.
//! - Every request is logged (method, target, headers, body SHA-256); every client
//!   disconnect before the response ended is logged with its `Instant` and phase.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

use crate::http;

pub const SECRET_PROMPT: &str = "SECRET-PROMPT-TEXT do not leak";

#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body_sha256: String,
    pub body_len: usize,
}

#[derive(Debug, Clone)]
pub struct Disconnect {
    pub at: Instant,
    /// "prefill" (before the first event) or "decode".
    pub phase: &'static str,
    pub events_sent: usize,
}

#[derive(Debug, Default)]
pub struct MockLog {
    pub requests: Vec<Seen>,
    pub disconnects: Vec<Disconnect>,
    pub completed: usize,
}

#[derive(Clone)]
pub struct MockLlama {
    pub addr: SocketAddr,
    pub log: Arc<Mutex<MockLog>>,
    pub changed: Arc<Notify>,
}

impl MockLlama {
    pub fn requests(&self) -> Vec<Seen> {
        self.log.lock().unwrap().requests.clone()
    }

    pub fn disconnects(&self) -> Vec<Disconnect> {
        self.log.lock().unwrap().disconnects.clone()
    }

    /// Wait until `n` disconnects were recorded (or the timeout).
    pub async fn wait_disconnects(&self, n: usize, timeout: Duration) -> Vec<Disconnect> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            let d = self.disconnects();
            if d.len() >= n {
                return d;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.disconnects();
            }
        }
    }
}

pub async fn start(bind: SocketAddr) -> io::Result<MockLlama> {
    let listener = TcpListener::bind(bind).await?;
    let mock = MockLlama {
        addr: listener.local_addr()?,
        log: Arc::default(),
        changed: Arc::new(Notify::new()),
    };
    let m = mock.clone();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                continue;
            };
            let m = m.clone();
            tokio::spawn(async move {
                let _ = serve_one(&m, sock).await;
            });
        }
    });
    Ok(mock)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Plain,
    Json,
    Tool,
}

pub fn variant_of(body: &[u8]) -> Variant {
    let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    if v.get("tool_choice").is_some_and(|t| t.is_object()) {
        Variant::Tool
    } else if v.get("response_format").is_some() {
        Variant::Json
    } else {
        Variant::Plain
    }
}

fn chunk_event(i: usize, delta: serde_json::Value, finish: Option<&str>) -> String {
    let obj = serde_json::json!({
        "choices": [{"finish_reason": finish, "index": 0, "delta": delta}],
        "created": 1764331200u64,
        "id": "chatcmpl-mock",
        "model": "slipstream",
        "system_fingerprint": "mock",
        "object": "chat.completion.chunk",
        "seq": i,
    });
    format!("data: {obj}\n\n")
}

/// The deterministic SSE events (without `[DONE]`) for a variant.
pub fn events(variant: Variant, n: usize) -> Vec<String> {
    let mut out = Vec::new();
    match variant {
        Variant::Plain => {
            out.push(chunk_event(0, serde_json::json!({"role":"assistant","content":null}), None));
            for i in 0..n {
                out.push(chunk_event(i + 1, serde_json::json!({"content": format!("tok{i} ")}), None));
            }
            out.push(chunk_event(n + 1, serde_json::json!({}), Some("length")));
        }
        Variant::Json => {
            let doc = format!(
                "{{\"answer\": 42, \"items\": [{}]}}",
                (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
            );
            let pieces: Vec<String> = doc
                .as_bytes()
                .chunks(3)
                .map(|c| String::from_utf8_lossy(c).into_owned())
                .collect();
            out.push(chunk_event(0, serde_json::json!({"role":"assistant","content":null}), None));
            for (i, p) in pieces.iter().enumerate() {
                out.push(chunk_event(i + 1, serde_json::json!({"content": p}), None));
            }
            out.push(chunk_event(pieces.len() + 1, serde_json::json!({}), Some("stop")));
        }
        Variant::Tool => {
            out.push(chunk_event(
                0,
                serde_json::json!({"role":"assistant","content":null,"tool_calls":[{"index":0,"id":"call_mock0","type":"function","function":{"name":"add","arguments":""}}]}),
                None,
            ));
            let args = format!("{{\"a\": 19, \"b\": 23, \"pad\": \"{}\"}}", "x".repeat(n));
            for (i, c) in args.as_bytes().chunks(4).enumerate() {
                out.push(chunk_event(
                    i + 1,
                    serde_json::json!({"tool_calls":[{"index":0,"function":{"arguments": String::from_utf8_lossy(c)}}]}),
                    None,
                ));
            }
            out.push(chunk_event(9999, serde_json::json!({}), Some("tool_calls")));
        }
    }
    out
}

pub const SSE_HEAD: &str = "HTTP/1.1 200 OK\r\nKeep-Alive: timeout=5, max=100\r\nContent-Type: text/event-stream\r\nServer: llama.cpp\r\nAccess-Control-Allow-Origin: \r\nTransfer-Encoding: chunked\r\n\r\n";

fn chunk(data: &str) -> Vec<u8> {
    let mut v = format!("{:x}\r\n", data.len()).into_bytes();
    v.extend_from_slice(data.as_bytes());
    v.extend_from_slice(b"\r\n");
    v
}

/// Exact raw bytes (head + chunked body) the mock sends for a streamed request.
pub fn expected_stream(variant: Variant, n: usize) -> Vec<u8> {
    let mut out = SSE_HEAD.as_bytes().to_vec();
    for e in events(variant, n) {
        out.extend_from_slice(&chunk(&e));
    }
    out.extend_from_slice(&chunk("data: [DONE]\n\n"));
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

#[derive(Debug, Clone, Copy)]
struct Params {
    n: usize,
    gap_us: u64,
    prefill_ms: u64,
    delay_ms: u64,
}

fn params(head: &http::RequestHead) -> Params {
    let mut p = Params {
        n: 64,
        gap_us: 0,
        prefill_ms: 0,
        delay_ms: 0,
    };
    let query = head.target.split_once('?').map(|(_, q)| q);
    for q in [query, head.header("x-mock-params")].into_iter().flatten() {
        for kv in q.split('&') {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            match k {
                "n" => p.n = v.parse().unwrap_or(p.n),
                "gap_us" => p.gap_us = v.parse().unwrap_or(0),
                "prefill_ms" => p.prefill_ms = v.parse().unwrap_or(0),
                "delay_ms" => p.delay_ms = v.parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    p
}

fn json(status: u16, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json; charset=utf-8\r\nServer: llama.cpp\r\nContent-Length: {}\r\n\r\n{body}",
        http::reason(status),
        body.len()
    )
    .into_bytes()
}

async fn read_body(sock: &mut TcpStream, head: &http::RequestHead, mut have: Vec<u8>) -> io::Result<Vec<u8>> {
    let mut buf = [0u8; 64 * 1024];
    if let Some(cl) = head.header("content-length") {
        let n: usize = cl.parse().unwrap_or(0);
        while have.len() < n {
            let k = sock.read(&mut buf).await?;
            if k == 0 {
                break;
            }
            have.extend_from_slice(&buf[..k]);
        }
        have.truncate(n);
        return Ok(have);
    }
    if head
        .header("transfer-encoding")
        .is_some_and(|v| v.contains("chunked"))
    {
        while !have.ends_with(b"0\r\n\r\n") {
            let k = sock.read(&mut buf).await?;
            if k == 0 {
                break;
            }
            have.extend_from_slice(&buf[..k]);
        }
        return Ok(http::dechunk_strict(&have, 64 * 1024 * 1024).unwrap_or_default());
    }
    Ok(Vec::new())
}

async fn serve_one(m: &MockLlama, mut sock: TcpStream) -> io::Result<()> {
    sock.set_nodelay(true)?;
    let Some((raw, rest)) = http::read_head(&mut sock).await? else {
        return Ok(());
    };
    let head = http::parse_request_head(&raw)?;
    let body = read_body(&mut sock, &head, rest).await?;
    m.log.lock().unwrap().requests.push(Seen {
        method: head.method.clone(),
        target: head.target.clone(),
        headers: head.headers.clone(),
        body_sha256: hex::encode(Sha256::digest(&body)),
        body_len: body.len(),
    });
    m.changed.notify_waiters();
    let path = head.target.split('?').next().unwrap_or("");
    let resp: Vec<u8> = match (head.method.as_str(), path) {
        ("POST", "/v1/chat/completions") | ("POST", "/v1/completions") => {
            let streamed = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
                .unwrap_or(false);
            if streamed {
                return stream(m, sock, variant_of(&body), params(&head)).await;
            }
            json(
                200,
                r#"{"choices":[{"finish_reason":"stop","index":0,"message":{"role":"assistant","content":"hello"}}],"created":1764331200,"id":"chatcmpl-mock","model":"slipstream","object":"chat.completion"}"#,
            )
        }
        ("GET", "/v1/models") => json(
            200,
            r#"{"object":"list","data":[{"id":"slipstream","object":"model","created":1764331200,"owned_by":"llamacpp","meta":{"n_ctx_train":262144}}]}"#,
        ),
        ("GET", "/health") => json(200, r#"{"status":"ok"}"#),
        ("GET", "/props") => json(
            200,
            r#"{"default_generation_settings":{"n_ctx":8192,"params":{"temperature":0}},"total_slots":1,"model_path":"/Users/x/Modelle/m.gguf","chat_template":"..."}"#,
        ),
        ("GET", "/metrics") => {
            let body = "# HELP llamacpp:prompt_tokens_total x\nllamacpp:prompt_tokens_total 1\n";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_bytes()
        }
        ("GET", "/slots") => {
            let slots = serde_json::json!([{
                "id": 0, "id_task": 7, "n_ctx": 8192, "speculative": false, "is_processing": true,
                "phase": "prompt", "prompt_progress": {"total": 1000, "cache": 0, "processed": 400, "time_ms": 1234.5},
                "prompt": SECRET_PROMPT, "next_token": [{"has_next_token": true, "n_remain": -1, "n_decoded": 0}],
                "params": {"grammar": SECRET_PROMPT, "temperature": 0.0}
            }]);
            // chunked, like some llama-server builds
            let body = slots.to_string();
            let mut v = b"HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
            v.extend_from_slice(&chunk(&body));
            v.extend_from_slice(b"0\r\n\r\n");
            v
        }
        ("POST", "/tokenize") => json(200, r#"{"tokens":[1,2,3]}"#),
        ("POST", "/detokenize") => json(200, r#"{"content":"abc"}"#),
        ("POST", "/apply-template") => json(200, r#"{"prompt":"<|user|>hi"}"#),
        ("POST", "/props") => json(200, r#"{"success":true}"#),
        ("POST", p) if p.starts_with("/slots/") => json(200, r#"{"id_slot":0,"filename":"x.bin","n_saved":1}"#),
        _ => json(404, r#"{"error":{"code":404,"message":"File Not Found","type":"not_found_error"}}"#),
    };
    let delay = params(&head).delay_ms;
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    sock.write_all(&resp).await?;
    sock.shutdown().await?;
    Ok(())
}

async fn stream(m: &MockLlama, sock: TcpStream, variant: Variant, p: Params) -> io::Result<()> {
    let (mut r, mut w) = sock.into_split();
    let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sent_w = sent.clone();
    let writer = async move {
        if p.prefill_ms > 0 {
            tokio::time::sleep(Duration::from_millis(p.prefill_ms)).await;
        }
        let evs = events(variant, p.n);
        let mut first = SSE_HEAD.as_bytes().to_vec();
        first.extend_from_slice(&chunk(&evs[0]));
        w.write_all(&first).await?;
        w.flush().await?;
        sent_w.store(1, std::sync::atomic::Ordering::SeqCst);
        for (i, e) in evs.iter().enumerate().skip(1) {
            if p.gap_us > 0 {
                tokio::time::sleep(Duration::from_micros(p.gap_us)).await;
            }
            w.write_all(&chunk(e)).await?;
            w.flush().await?;
            sent_w.store(i + 1, std::sync::atomic::Ordering::SeqCst);
        }
        w.write_all(&chunk("data: [DONE]\n\n")).await?;
        w.write_all(b"0\r\n\r\n").await?;
        w.flush().await?;
        w.shutdown().await?;
        io::Result::Ok(())
    };
    let watcher = async move {
        let mut buf = [0u8; 1024];
        loop {
            match r.read(&mut buf).await {
                Ok(0) | Err(_) => return Instant::now(),
                Ok(_) => {}
            }
        }
    };
    tokio::select! {
        res = writer => {
            if res.is_ok() {
                m.log.lock().unwrap().completed += 1;
            } else {
                let n = sent.load(std::sync::atomic::Ordering::SeqCst);
                m.log.lock().unwrap().disconnects.push(Disconnect {
                    at: Instant::now(),
                    phase: if n == 0 { "prefill" } else { "decode" },
                    events_sent: n,
                });
            }
            m.changed.notify_waiters();
        }
        at = watcher => {
            let n = sent.load(std::sync::atomic::Ordering::SeqCst);
            m.log.lock().unwrap().disconnects.push(Disconnect {
                at,
                phase: if n == 0 { "prefill" } else { "decode" },
                events_sent: n,
            });
            m.changed.notify_waiters();
        }
    }
    Ok(())
}
