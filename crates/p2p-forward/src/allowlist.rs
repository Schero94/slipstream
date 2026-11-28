//! Method/path allowlist, header stripping, local-origin checks and `/slots`
//! sanitization (plan §3 `allowlist.rs`, §8). Checked on **both** ends: the use side
//! refuses before opening a stream, the share side refuses again before touching
//! llama-server (a modified or buggy client cannot reach `POST /slots/*` file writes).

use serde_json::{Map, Value};

/// What the share side does with an allowed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Uses the model slot (`POST /v1/chat/completions`, `POST /v1/completions`):
    /// subject to per-peer admission (1 in flight + 2 queued).
    Inference,
    /// Cheap request forwarded as-is (models, health, tokenize, metrics, props).
    Light,
    /// `GET /slots`: buffered and reduced to non-sensitive fields.
    SlotsSanitized,
}

/// Request path without query string.
pub fn path_of(target: &str) -> &str {
    target.split(['?', '#']).next().unwrap_or("")
}

/// The exact allowlist from the plan. Anything else → `None` (403
/// "Not available through a shared Mac"), including `POST /slots/*`, `POST /props`,
/// `GET /slots/<id>`, other methods, and any encoded/odd spelling of a path (exact
/// string comparison, no normalization).
pub fn route(method: &str, target: &str) -> Option<Route> {
    if target.contains('#') || !target.starts_with('/') {
        return None;
    }
    let path = path_of(target);
    match (method, path) {
        ("POST", "/v1/chat/completions") | ("POST", "/v1/completions") => Some(Route::Inference),
        ("GET", "/v1/models")
        | ("GET", "/health")
        | ("POST", "/tokenize")
        | ("POST", "/detokenize")
        | ("POST", "/apply-template")
        | ("GET", "/metrics")
        | ("GET", "/props") => Some(Route::Light),
        ("GET", "/slots") => Some(Route::SlotsSanitized),
        _ => None,
    }
}

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Headers never forwarded: credentials, hop-by-hop, framing and routing headers (the
/// share side sets its own `Host`, `Content-Length`/`Transfer-Encoding` and
/// `Connection: close`), `Expect` (answered locally), browser-origin headers.
const STRIPPED: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "host",
    "content-length",
    "expect",
    "origin",
    "referer",
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
];

/// Filter request headers (names already lower-case). Also drops every header named in
/// `Connection:` (RFC 9110 §7.6.1).
pub fn filter_request_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let connection_named: Vec<String> = headers
        .iter()
        .filter(|(k, _)| k == "connection")
        .flat_map(|(_, v)| v.split(',').map(|t| t.trim().to_ascii_lowercase()))
        .collect();
    headers
        .iter()
        .filter(|(k, _)| {
            let k = k.as_str();
            !HOP_BY_HOP.contains(&k)
                && !STRIPPED.contains(&k)
                && !k.starts_with("sec-")
                && !k.starts_with("proxy-")
                && !connection_named.iter().any(|c| c == k)
                && valid_token(k)
        })
        .filter(|(_, v)| !v.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0))
        .cloned()
        .collect()
}

fn valid_token(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// DNS-rebinding guard: `Host` must name loopback on the bound port (or no port).
pub fn host_allowed(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else {
        return false;
    };
    let host = host.trim().to_ascii_lowercase();
    let (name, host_port) = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((name, tail)) => (format!("[{name}]"), tail.strip_prefix(':')),
            None => return false,
        }
    } else {
        match host.rsplit_once(':') {
            Some((name, p)) => (name.to_string(), Some(p)),
            None => (host.clone(), None),
        }
    };
    let port_ok = match host_port {
        None => true,
        Some(p) => p.parse::<u16>().ok() == Some(port),
    };
    port_ok && matches!(name.as_str(), "127.0.0.1" | "localhost" | "[::1]")
}

/// Browser guard: requests carrying a non-null `Origin` (a web page) are refused, and so
/// are requests a browser marks as cross-site (`Sec-Fetch-Site`, which also covers the
/// `Origin: null` of sandboxed frames). Editors (Node/Rust HTTP clients) send neither.
pub fn browser_origin_refused(origin: Option<&str>, sec_fetch_site: Option<&str>) -> bool {
    if let Some(o) = origin {
        if o.trim() != "null" {
            return true;
        }
    }
    if let Some(site) = sec_fetch_site {
        let site = site.trim().to_ascii_lowercase();
        if site != "none" && site != "same-origin" {
            return true;
        }
    }
    false
}

/// Fields of one `/slots` entry that may leave the big Mac (no prompt, no tokens, no
/// sampling params, no generated text).
pub const SLOT_FIELDS: &[&str] = &["id", "is_processing", "phase", "prompt_progress", "n_ctx"];

/// Reduce a `/slots` JSON document to [`SLOT_FIELDS`]. `prompt_progress` keeps only its
/// numeric members. Error objects keep only `code`/`message`/`type`; anything else
/// becomes `[]`.
pub fn sanitize_slots(body: &[u8]) -> Vec<u8> {
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let out = match value {
        Value::Array(slots) => Value::Array(
            slots
                .into_iter()
                .filter_map(|slot| match slot {
                    Value::Object(obj) => Some(Value::Object(sanitize_slot(obj))),
                    _ => None,
                })
                .collect(),
        ),
        Value::Object(obj) if obj.get("error").is_some_and(Value::is_object) => {
            let err = obj["error"].as_object().cloned().unwrap_or_default();
            let mut e = Map::new();
            for k in ["code", "message", "type"] {
                if let Some(v) = err.get(k) {
                    if v.is_number() || v.is_string() {
                        e.insert(k.to_string(), v.clone());
                    }
                }
            }
            serde_json::json!({ "error": e })
        }
        _ => Value::Array(Vec::new()),
    };
    serde_json::to_vec(&out).expect("json")
}

fn sanitize_slot(obj: Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for key in SLOT_FIELDS {
        let Some(v) = obj.get(*key) else { continue };
        let keep = match *key {
            "id" | "n_ctx" => v.is_number().then(|| v.clone()),
            "is_processing" => v.is_boolean().then(|| v.clone()),
            "phase" => v
                .as_str()
                .filter(|p| matches!(*p, "idle" | "waiting" | "prompt" | "generating"))
                .map(|p| Value::String(p.to_string())),
            "prompt_progress" => v.as_object().map(|pp| {
                Value::Object(
                    pp.iter()
                        .filter(|(_, x)| x.is_number())
                        .map(|(k, x)| (k.clone(), x.clone()))
                        .collect(),
                )
            }),
            _ => None,
        };
        if let Some(v) = keep {
            out.insert(key.to_string(), v);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_allowlist() {
        assert_eq!(route("POST", "/v1/chat/completions"), Some(Route::Inference));
        assert_eq!(route("POST", "/v1/completions"), Some(Route::Inference));
        assert_eq!(route("GET", "/v1/models"), Some(Route::Light));
        assert_eq!(route("GET", "/health"), Some(Route::Light));
        for p in ["/tokenize", "/detokenize", "/apply-template"] {
            assert_eq!(route("POST", p), Some(Route::Light));
        }
        assert_eq!(route("GET", "/metrics"), Some(Route::Light));
        assert_eq!(route("GET", "/props"), Some(Route::Light));
        assert_eq!(route("GET", "/slots"), Some(Route::SlotsSanitized));
        assert_eq!(route("GET", "/slots?fail_on_no_slot=1"), Some(Route::SlotsSanitized));
        for (m, p) in [
            ("POST", "/slots/0?action=save"),
            ("POST", "/slots/0?action=restore"),
            ("POST", "/slots/0?action=erase"),
            ("POST", "/slots"),
            ("GET", "/slots/0"),
            ("POST", "/props"),
            ("GET", "/v1/chat/completions"),
            ("POST", "/chat/completions"),
            ("POST", "/completion"),
            ("POST", "/v1/chat/completions/"),
            ("POST", "//v1/chat/completions"),
            ("POST", "/v1/chat%2Fcompletions"),
            ("POST", "/v1/../slots/0"),
            ("POST", "/infill"),
            ("POST", "/lora-adapters"),
            ("GET", "/"),
            ("OPTIONS", "/v1/chat/completions"),
            ("HEAD", "/health"),
            ("DELETE", "/slots"),
            ("post", "/v1/chat/completions"),
            ("GET", "http://127.0.0.1/health"),
            ("GET", "/health#x"),
        ] {
            assert_eq!(route(m, p), None, "{m} {p} must be refused");
        }
    }

    #[test]
    fn header_filter_strips_credentials_and_hop_by_hop() {
        let h = |k: &str, v: &str| (k.to_string(), v.to_string());
        let out = filter_request_headers(&[
            h("authorization", "Bearer sk-secret"),
            h("cookie", "a=b"),
            h("proxy-authorization", "x"),
            h("connection", "keep-alive, x-private"),
            h("x-private", "1"),
            h("keep-alive", "timeout=5"),
            h("transfer-encoding", "chunked"),
            h("upgrade", "h2c"),
            h("te", "trailers"),
            h("host", "127.0.0.1:8080"),
            h("content-length", "5"),
            h("expect", "100-continue"),
            h("origin", "null"),
            h("sec-fetch-site", "none"),
            h("content-type", "application/json"),
            h("accept", "text/event-stream"),
            h("x-evil", "a\r\nInjected: 1"),
        ]);
        assert_eq!(
            out,
            vec![h("content-type", "application/json"), h("accept", "text/event-stream")]
        );
    }

    #[test]
    fn host_and_origin_guards() {
        assert!(host_allowed(Some("127.0.0.1:8080"), 8080));
        assert!(host_allowed(Some("localhost:8080"), 8080));
        assert!(host_allowed(Some("LOCALHOST"), 8080));
        assert!(host_allowed(Some("[::1]:8080"), 8080));
        assert!(!host_allowed(Some("127.0.0.1:9999"), 8080));
        assert!(!host_allowed(Some("evil.example:8080"), 8080));
        assert!(!host_allowed(Some("127.0.0.1.evil.example"), 8080));
        assert!(!host_allowed(Some("[::1"), 8080));
        assert!(!host_allowed(None, 8080));
        assert!(!browser_origin_refused(None, None));
        assert!(!browser_origin_refused(Some("null"), None));
        assert!(!browser_origin_refused(None, Some("none")));
        assert!(browser_origin_refused(Some("https://evil.example"), None));
        assert!(browser_origin_refused(Some("http://127.0.0.1:8080"), None));
        assert!(browser_origin_refused(Some("null"), Some("cross-site")));
    }

    #[test]
    fn slots_sanitized() {
        let raw = br#"[{"id":0,"n_ctx":8192,"is_processing":true,"phase":"prompt",
            "prompt_progress":{"total":100,"cache":0,"processed":40,"time_ms":12.5,"note":"x"},
            "prompt":"SECRET","next_token":[{"text":"SECRET"}],"params":{"grammar":"SECRET"},
            "speculative":false,"id_task":7}, 5]"#;
        let out = String::from_utf8(sanitize_slots(raw)).unwrap();
        assert!(!out.contains("SECRET"));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v,
            serde_json::json!([{"id":0,"n_ctx":8192,"is_processing":true,"phase":"prompt",
                "prompt_progress":{"total":100,"cache":0,"processed":40,"time_ms":12.5}}])
        );
        let err = sanitize_slots(br#"{"error":{"code":501,"message":"off","type":"x","detail":"SECRET"}}"#);
        assert!(!String::from_utf8_lossy(&err).contains("SECRET"));
        assert_eq!(sanitize_slots(b"not json"), b"[]");
        assert_eq!(sanitize_slots(br#"[{"phase":"SECRET"}]"#), b"[{}]");
    }
}
