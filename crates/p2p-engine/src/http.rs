//! OpenAI-compatible HTTP inference for oMLX / llama-server / Slipstream.
//!
//! POSTs non-streaming `POST {endpoint}/v1/chat/completions` (same shape as
//! `apps/peregrine-control` chat / test panel). Used by [`crate::MlxEngine`] and
//! [`crate::LlamaPgrnEngine`] when an endpoint is configured.

use std::time::Duration;

use p2p_core::{BackendKind, InferenceEngine, JobRequest, JobResult};
use serde_json::{json, Value};

/// Default connect timeout — fail fast when the server is down (no hang).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Default read timeout for a full completion (GPU work can be slow).
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(180);
/// Slipstream / oMLX local API key (same as peregrine-control).
pub const DEFAULT_API_KEY: &str = "sk-local";

/// OpenAI-compatible chat-completions client.
#[derive(Debug, Clone)]
pub struct HttpEngine {
    pub endpoint: String,
    pub api_key: String,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    /// Optional backend label for capability advert (None for bare HTTP).
    pub backend_kind: Option<BackendKind>,
}

impl HttpEngine {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: normalize_endpoint(&endpoint.into()),
            api_key: api_key_from_env(),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            read_timeout: read_timeout_from_env(),
            backend_kind: None,
        }
    }

    pub fn with_backend(mut self, kind: BackendKind) -> Self {
        self.backend_kind = Some(kind);
        self
    }

    pub fn with_timeouts(mut self, connect: Duration, read: Duration) -> Self {
        self.connect_timeout = connect;
        self.read_timeout = read;
        self
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = key.into();
        self
    }

    pub fn chat_url(&self) -> String {
        chat_completions_url(&self.endpoint)
    }
}

impl InferenceEngine for HttpEngine {
    fn infer(&self, job: &JobRequest) -> JobResult {
        http_chat_completions(
            &self.endpoint,
            Some(&self.api_key),
            self.connect_timeout,
            self.read_timeout,
            job,
        )
    }

    fn backend_kind(&self) -> Option<BackendKind> {
        self.backend_kind
    }
}

/// Strip trailing slashes from a base URL.
pub fn normalize_endpoint(base: &str) -> String {
    let mut s = base.trim().to_string();
    while s.ends_with('/') {
        s.pop();
    }
    s
}

/// `http://{host}:{port}` (host may already include brackets for IPv6).
pub fn local_endpoint(host: &str, port: u16) -> String {
    format!("http://{host}:{port}")
}

pub fn chat_completions_url(endpoint: &str) -> String {
    format!("{}/v1/chat/completions", normalize_endpoint(endpoint))
}

pub fn api_key_from_env() -> String {
    std::env::var("P2P_API_KEY")
        .or_else(|_| std::env::var("SLIPSTREAM_API_KEY"))
        .unwrap_or_else(|_| DEFAULT_API_KEY.to_string())
}

pub fn read_timeout_from_env() -> Duration {
    std::env::var("P2P_HTTP_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_READ_TIMEOUT)
}

/// Build the OpenAI chat-completions JSON body (non-streaming).
pub fn build_chat_body(job: &JobRequest) -> Value {
    let mut messages = Vec::new();
    if !job.system.trim().is_empty() {
        messages.push(json!({
            "role": "system",
            "content": job.system,
        }));
    }
    messages.push(json!({
        "role": "user",
        "content": job.prompt,
    }));
    json!({
        "model": job.model,
        "messages": messages,
        "max_tokens": job.max_tokens,
        "stream": false,
        "temperature": 0,
    })
}

/// POST `{endpoint}/v1/chat/completions` and map the response to [`JobResult`].
pub fn http_chat_completions(
    endpoint: &str,
    api_key: Option<&str>,
    connect_timeout: Duration,
    read_timeout: Duration,
    job: &JobRequest,
) -> JobResult {
    let url = chat_completions_url(endpoint);
    let body = build_chat_body(job);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(connect_timeout)
        .timeout_read(read_timeout)
        .timeout_write(connect_timeout)
        .build();

    let mut req = agent
        .post(&url)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json");
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }

    let response = match req.send_json(body) {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            let detail = truncate(&detail, 512);
            return JobResult::failure(
                &job.job_id,
                format!("HTTP {code} from {url}: {detail}"),
            );
        }
        Err(ureq::Error::Transport(t)) => {
            return JobResult::failure(
                &job.job_id,
                format!("HTTP transport error talking to {url}: {t}"),
            );
        }
    };

    let status = response.status();
    let text = match response.into_string() {
        Ok(t) => t,
        Err(e) => {
            return JobResult::failure(
                &job.job_id,
                format!("failed to read response from {url}: {e}"),
            );
        }
    };

    if !(200..300).contains(&status) {
        return JobResult::failure(
            &job.job_id,
            format!("HTTP {status} from {url}: {}", truncate(&text, 512)),
        );
    }

    parse_chat_completions_response(&job.job_id, &text)
}

/// Parse an OpenAI-style chat completions JSON body.
pub fn parse_chat_completions_response(job_id: &str, body: &str) -> JobResult {
    let v: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            return JobResult::failure(
                job_id,
                format!("invalid JSON from chat completions: {e}"),
            );
        }
    };

    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or_else(|| err.as_str().unwrap_or("unknown API error"));
        return JobResult::failure(job_id, format!("API error: {msg}"));
    }

    let text = v
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .or_else(|| {
            // Some servers put text on delta-style or plain text fields.
            v.pointer("/choices/0/text").and_then(|c| c.as_str())
        })
        .unwrap_or("")
        .to_string();

    if text.is_empty()
        && v.pointer("/choices/0")
            .and_then(|c| c.get("finish_reason"))
            .is_none()
        && v.get("choices").is_none()
    {
        return JobResult::failure(
            job_id,
            format!(
                "chat completions response missing choices: {}",
                truncate(body, 256)
            ),
        );
    }

    let tokens = v
        .pointer("/usage/completion_tokens")
        .and_then(|t| t.as_u64())
        .or_else(|| v.pointer("/usage/total_tokens").and_then(|t| t.as_u64()))
        .unwrap_or_else(|| estimate_tokens(&text)) as u32;

    JobResult::success(job_id, text, tokens)
}

fn estimate_tokens(text: &str) -> u64 {
    let n = text.split_whitespace().count() as u64;
    n.max(if text.is_empty() { 0 } else { 1 })
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

/// Shared path for MLX / Llama adapters when an endpoint is set.
pub fn infer_via_http(
    endpoint: &str,
    backend: BackendKind,
    job: &JobRequest,
) -> JobResult {
    HttpEngine::new(endpoint)
        .with_backend(backend)
        .infer(job)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    /// Tiny one-shot HTTP server for unit tests (no GPU, no hyper).
    fn spawn_mock_server(
        status_line: &str,
        response_body: &str,
        expect_auth: Option<&str>,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let status = status_line.to_string();
        let body = response_body.to_string();
        let expect_auth = expect_auth.map(|s| s.to_string());
        let (ready_tx, ready_rx) = mpsc::channel();

        let handle = thread::spawn(move || {
            ready_tx.send(()).ok();
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut request = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).expect("read") == 0 {
                    break;
                }
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            // Drain Content-Length body if present.
            let content_len = request.lines().find_map(|l| {
                let lower = l.to_ascii_lowercase();
                lower
                    .strip_prefix("content-length:")
                    .and_then(|s| s.trim().parse::<usize>().ok())
            });
            if let Some(cl) = content_len {
                let mut buf = vec![0u8; cl];
                reader.read_exact(&mut buf).ok();
                request.push_str(&String::from_utf8_lossy(&buf));
            }

            assert!(
                request.contains("POST /v1/chat/completions"),
                "unexpected request:\n{request}"
            );
            assert!(request.contains("\"stream\":false") || request.contains("\"stream\": false"));
            if let Some(ref auth) = expect_auth {
                assert!(
                    request.contains(&format!("Authorization: Bearer {auth}")),
                    "missing auth in:\n{request}"
                );
            }

            let resp = format!(
                "{status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).expect("write");
        });

        ready_rx.recv().expect("server ready");
        (format!("http://{addr}"), handle)
    }

    fn sample_job() -> JobRequest {
        JobRequest {
            job_id: "job-1".into(),
            model: "slipstream".into(),
            system: "be brief".into(),
            prompt: "hello".into(),
            max_tokens: 16,
        }
    }

    #[test]
    fn build_chat_body_includes_system_and_user() {
        let body = build_chat_body(&sample_job());
        assert_eq!(body["model"], "slipstream");
        assert_eq!(body["max_tokens"], 16);
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "hello");
    }

    #[test]
    fn build_chat_body_omits_empty_system() {
        let mut job = sample_job();
        job.system.clear();
        let body = build_chat_body(&job);
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn parse_success_with_usage() {
        let body = r#"{
            "choices": [{"message": {"role": "assistant", "content": "hi there"}}],
            "usage": {"completion_tokens": 2, "total_tokens": 5}
        }"#;
        let r = parse_chat_completions_response("j", body);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.text, "hi there");
        assert_eq!(r.tokens, 2);
    }

    #[test]
    fn parse_api_error_object() {
        let body = r#"{"error":{"message":"model not found"}}"#;
        let r = parse_chat_completions_response("j", body);
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("model not found"));
    }

    #[test]
    fn http_infer_success_against_localhost_mock() {
        let json = r#"{
            "id":"chatcmpl-test",
            "choices":[{"index":0,"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}
        }"#;
        let (base, handle) = spawn_mock_server("HTTP/1.1 200 OK", json, Some(DEFAULT_API_KEY));
        let eng = HttpEngine::new(&base)
            .with_timeouts(Duration::from_secs(2), Duration::from_secs(5))
            .with_api_key(DEFAULT_API_KEY);
        let r = eng.infer(&sample_job());
        handle.join().expect("server");
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.text, "pong");
        assert_eq!(r.tokens, 1);
        assert_eq!(r.job_id, "job-1");
    }

    #[test]
    fn http_infer_maps_http_error() {
        let (base, handle) =
            spawn_mock_server("HTTP/1.1 503 Service Unavailable", r#"{"error":"busy"}"#, None);
        let eng = HttpEngine::new(&base)
            .with_timeouts(Duration::from_secs(2), Duration::from_secs(5))
            .with_api_key("");
        let r = eng.infer(&sample_job());
        handle.join().expect("server");
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert!(err.contains("503") || err.contains("HTTP"), "{err}");
    }

    #[test]
    fn http_infer_connection_refused_is_clear() {
        // Nothing listening — must fail fast with a transport error.
        let eng = HttpEngine::new("http://127.0.0.1:1")
            .with_timeouts(Duration::from_millis(500), Duration::from_secs(1));
        let r = eng.infer(&sample_job());
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert!(
            err.contains("transport") || err.contains("Connection") || err.contains("connect"),
            "{err}"
        );
    }

    #[test]
    fn normalize_and_url_helpers() {
        assert_eq!(
            chat_completions_url("http://127.0.0.1:8080/"),
            "http://127.0.0.1:8080/v1/chat/completions"
        );
        assert_eq!(
            local_endpoint("127.0.0.1", 8080),
            "http://127.0.0.1:8080"
        );
    }
}
