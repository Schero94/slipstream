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
    /// Cheap request forwarded as-is (models, health, tokenize, metrics), subject to the
    /// light-route admission (per peer + global cap).
    Light,
    /// `GET /slots`: buffered and reduced to non-sensitive fields.
    SlotsSanitized,
    /// `GET /props`: buffered and reduced to [`PROPS_FIELDS`] (no model path, no chat
    /// template).
    PropsSanitized,
}

/// Request path without query string.
pub fn path_of(target: &str) -> &str {
    target.split(['?', '#']).next().unwrap_or("")
}

/// Every byte of a request target must be visible US-ASCII (`0x21..=0x7E`): no space,
/// no CR/LF, no NUL, no control or non-ASCII byte (e.g. U+0085 NEL). Checked on both
/// ends; this alone already makes request-line injection impossible.
pub fn target_bytes_ok(target: &str) -> bool {
    !target.is_empty() && target.bytes().all(|b| (0x21..=0x7E).contains(&b))
}

/// The allowlist: (method, path, route). Paths are `'static` constants; the share side
/// writes **these** into the upstream request line, never the peer's bytes.
const ROUTES: &[(&str, &str, Route)] = &[
    ("POST", "/v1/chat/completions", Route::Inference),
    ("POST", "/v1/completions", Route::Inference),
    ("GET", "/v1/models", Route::Light),
    ("GET", "/health", Route::Light),
    ("POST", "/tokenize", Route::Light),
    ("POST", "/detokenize", Route::Light),
    ("POST", "/apply-template", Route::Light),
    ("GET", "/metrics", Route::Light),
    ("GET", "/props", Route::PropsSanitized),
    ("GET", "/slots", Route::SlotsSanitized),
];

/// An allowed request, rebuilt from constants: method, path and (only for `GET /slots`)
/// the one permitted query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Canonical {
    pub method: &'static str,
    pub path: &'static str,
    pub query: Option<&'static str>,
    pub route: Route,
}

impl Canonical {
    /// The request target written upstream (origin-form, constant bytes only).
    pub fn target(&self) -> String {
        match self.query {
            Some(q) => format!("{}?{q}", self.path),
            None => self.path.to_string(),
        }
    }
}

/// Match a request against the allowlist and rebuild it from constants. The query string
/// is dropped, except `GET /slots?fail_on_no_slot=0|1` (the only query llama-server's
/// allowlisted endpoints act on). Anything else → `None` (403 "Not available through a
/// shared Mac"), including `POST /slots/*`, `POST /props`, `GET /slots/<id>`, other
/// methods, any encoded/odd spelling of a path (exact string comparison, no
/// normalization), fragments, and any target byte outside `0x21..=0x7E`.
pub fn canonical(method: &str, target: &str) -> Option<Canonical> {
    if !target_bytes_ok(target) || target.contains('#') || !target.starts_with('/') {
        return None;
    }
    let path = path_of(target);
    let query = target.split_once('?').map(|(_, q)| q);
    let &(m, p, route) = ROUTES
        .iter()
        .find(|(m, p, _)| *m == method && *p == path)?;
    let query = match (route, query) {
        (Route::SlotsSanitized, Some("fail_on_no_slot=0")) => Some("fail_on_no_slot=0"),
        (Route::SlotsSanitized, Some("fail_on_no_slot=1")) => Some("fail_on_no_slot=1"),
        _ => None,
    };
    Some(Canonical {
        method: m,
        path: p,
        query,
        route,
    })
}

/// [`canonical`]'s route only.
pub fn route(method: &str, target: &str) -> Option<Route> {
    canonical(method, target).map(|c| c.route)
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
        .filter(|(_, v)| header_value_ok(v))
        .cloned()
        .collect()
}

/// Header values: no control bytes except HTAB (so no CR/LF/NUL injection), no DEL.
fn header_value_ok(v: &str) -> bool {
    !v.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7F)
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

/// Browser guard (M1 security review): **any** `Origin` header refuses the request —
/// including `Origin: null` (sandboxed frames, `file://`, redirects) — and so does any
/// `Sec-Fetch-Site` other than `none` (user-initiated navigation). Every instance of
/// both headers is checked (`headers` = all request headers, names lower-case).
/// Editors (Node/Rust/Python HTTP clients) send neither header. `Sec-Fetch-Mode` is
/// deliberately **not** a browser signal: Node's undici `fetch` sends
/// `sec-fetch-mode: cors` on every request.
pub fn browser_origin_refused(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(k, v)| match k.as_str() {
        "origin" => true,
        "sec-fetch-site" => !v.trim().eq_ignore_ascii_case("none"),
        _ => false,
    })
}

/// Fields of `GET /props` that may leave the big Mac. `model_path` (a filesystem path)
/// and `chat_template` never do; the model is named by its alias or file name only.
pub const PROPS_FIELDS: &[&str] = &[
    "default_generation_settings.n_ctx",
    "n_ctx",
    "total_slots",
    "model_alias",
    "model_name",
    "build_info",
];

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or("")
}

fn short_text(v: &Value) -> Option<Value> {
    v.as_str()
        .filter(|s| s.len() <= 256 && !s.chars().any(char::is_control))
        .map(|s| Value::String(s.to_string()))
}

/// Reduce a `/props` JSON document to [`PROPS_FIELDS`]. Non-object documents → `{}`
/// (error objects keep only `code`/`message`/`type`, like `/slots`).
pub fn sanitize_props(body: &[u8]) -> Vec<u8> {
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let Value::Object(obj) = value else {
        return b"{}".to_vec();
    };
    if obj.get("error").is_some_and(Value::is_object) {
        return sanitize_slots(&serde_json::to_vec(&obj).expect("json"));
    }
    let mut out = Map::new();
    let n_ctx = obj
        .get("default_generation_settings")
        .and_then(|d| d.get("n_ctx"))
        .filter(|v| v.is_u64())
        .cloned();
    if let Some(n) = n_ctx {
        out.insert(
            "default_generation_settings".into(),
            serde_json::json!({ "n_ctx": n }),
        );
    }
    if let Some(n) = obj.get("n_ctx").filter(|v| v.is_u64()) {
        out.insert("n_ctx".into(), n.clone());
    }
    if let Some(n) = obj.get("total_slots").filter(|v| v.is_u64()) {
        out.insert("total_slots".into(), n.clone());
    }
    if let Some(a) = obj
        .get("model_alias")
        .and_then(short_text)
        .filter(|a| !a.as_str().unwrap_or("").contains(['/', '\\']))
    {
        out.insert("model_alias".into(), a);
    }
    if let Some(name) = obj
        .get("model_path")
        .and_then(Value::as_str)
        .map(file_name)
        .filter(|n| !n.is_empty())
        .and_then(|n| short_text(&Value::String(n.to_string())))
    {
        out.insert("model_name".into(), name);
    }
    if let Some(b) = obj.get("build_info").and_then(short_text) {
        out.insert("build_info".into(), b);
    }
    serde_json::to_vec(&Value::Object(out)).expect("json")
}

/// Text received from the other Mac (e.g. its status `name`) made safe to print on a
/// terminal: control characters (C0, DEL, C1 incl. U+0085) and bidi/format overrides
/// removed, at most 64 characters.
pub fn display_safe(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
        })
        .take(64)
        .collect()
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
        assert_eq!(route("GET", "/props"), Some(Route::PropsSanitized));
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
        let h = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
        };
        assert!(!browser_origin_refused(&h(&[])));
        assert!(!browser_origin_refused(&h(&[("sec-fetch-site", "none")])));
        // Node's undici fetch: sec-fetch-mode is not a browser signal
        assert!(!browser_origin_refused(&h(&[("sec-fetch-mode", "cors"), ("user-agent", "node")])));
        assert!(browser_origin_refused(&h(&[("origin", "null")])), "Origin: null is refused");
        assert!(browser_origin_refused(&h(&[("origin", "")])));
        assert!(browser_origin_refused(&h(&[("origin", "https://evil.example")])));
        assert!(browser_origin_refused(&h(&[("origin", "http://127.0.0.1:8080")])));
        assert!(browser_origin_refused(&h(&[("sec-fetch-site", "same-origin")])));
        assert!(browser_origin_refused(&h(&[("sec-fetch-site", "cross-site")])));
        // every instance counts, not only the first
        assert!(browser_origin_refused(&h(&[("sec-fetch-site", "none"), ("sec-fetch-site", "cross-site")])));
        assert!(browser_origin_refused(&h(&[("x-a", "1"), ("origin", "null")])));
    }

    /// M1 review finding 1 (CRITICAL): a paired peer must not be able to smuggle a second
    /// upstream request through `ReqHead.target`.
    #[test]
    fn target_bytes_and_canonical_rebuild() {
        for bad in [
            "/health?a HTTP/1.1\r\nHost: x\r\n\r\nPOST /slots/0?action=save HTTP/1.1",
            "/health\r\n",
            "/health?x\ny",
            "/health?a b",
            "/health ",
            "/health?\u{0085}",
            "/health\0",
            "/health?\x7f",
            "/health\t",
            "",
        ] {
            assert!(!target_bytes_ok(bad), "{bad:?}");
            assert_eq!(canonical("GET", bad), None, "{bad:?}");
        }
        assert!(target_bytes_ok("/v1/chat/completions?n=1&x=%0d%0a"));
        // the query is dropped; the path written upstream is the route constant
        let c = canonical("POST", "/v1/chat/completions?n=2&x=%0d%0aPOST%20/slots/0").unwrap();
        assert_eq!((c.method, c.target()), ("POST", "/v1/chat/completions".to_string()));
        assert_eq!(canonical("GET", "/health?x=1").unwrap().target(), "/health");
        assert_eq!(canonical("GET", "/props?x=1").unwrap().target(), "/props");
        // GET /slots keeps only fail_on_no_slot=0|1
        assert_eq!(canonical("GET", "/slots?fail_on_no_slot=1").unwrap().target(), "/slots?fail_on_no_slot=1");
        assert_eq!(canonical("GET", "/slots?fail_on_no_slot=0").unwrap().target(), "/slots?fail_on_no_slot=0");
        assert_eq!(canonical("GET", "/slots?fail_on_no_slot=2").unwrap().target(), "/slots");
        assert_eq!(canonical("GET", "/slots?fail_on_no_slot=1&id=0").unwrap().target(), "/slots");
        assert_eq!(canonical("GET", "/slots?id_slot=0&action=save").unwrap().target(), "/slots");
        // constants are 'static and themselves pass the byte check
        for (m, p, _) in ROUTES {
            assert!(target_bytes_ok(p) && target_bytes_ok(m));
        }
    }

    #[test]
    fn props_sanitized() {
        let raw = br#"{"default_generation_settings":{"n_ctx":8192,"params":{"temperature":0,"grammar":"SECRET"}},
            "total_slots":1,"model_path":"/Users/SECRET/Modelle/groot-19b.gguf","model_alias":"groot",
            "chat_template":"SECRET {{messages}}","build_info":"b1234-abc","system_prompt":"SECRET","n_ctx":8192}"#;
        let out = String::from_utf8(sanitize_props(raw)).unwrap();
        assert!(!out.contains("SECRET") && !out.contains("/Users"), "{out}");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"default_generation_settings":{"n_ctx":8192},"n_ctx":8192,"total_slots":1,
                "model_alias":"groot","model_name":"groot-19b.gguf","build_info":"b1234-abc"})
        );
        assert_eq!(sanitize_props(br#"{"model_alias":"/Users/x/m.gguf"}"#), b"{}");
        assert_eq!(sanitize_props(b"[1]"), b"{}");
        assert_eq!(sanitize_props(b"nope"), b"{}");
        let err = sanitize_props(br#"{"error":{"code":503,"message":"loading","detail":"SECRET"}}"#);
        assert!(!String::from_utf8_lossy(&err).contains("SECRET"));
    }

    #[test]
    fn display_safe_strips_control_characters() {
        assert_eq!(display_safe("Studio\x1b[2J\x1b]0;pwned\x07"), "Studio[2J]0;pwned");
        assert_eq!(display_safe("a\r\nb\u{0085}c\u{202E}d\0"), "abcd");
        assert_eq!(display_safe("Jörg’s Mac Studio"), "Jörg’s Mac Studio");
        assert_eq!(display_safe(&"x".repeat(500)).len(), 64);
    }

    #[test]
    fn header_values_with_control_bytes_dropped() {
        let h = |k: &str, v: &str| (k.to_string(), v.to_string());
        let out = filter_request_headers(&[
            h("x-a", "ok\tvalue"),
            h("x-b", "bad\x0bvalue"),
            h("x-c", "bad\x7f"),
            h("x-d", "bad\0"),
        ]);
        assert_eq!(out, vec![h("x-a", "ok\tvalue")]);
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
