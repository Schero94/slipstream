//! Status RPC over `/slipstream/status/1` (plan §3 `status.rs`).
//!
//! The use side sends one `StatusReq` frame (JSON [`StatusRequest`]), the share side
//! answers with one `StatusResp` frame (JSON [`StatusReply`]) and FIN. `StartModel` /
//! `StopModel` are answered with a clear refusal in the headless share; the app (M4)
//! passes a [`ModelControl`] that starts the model through its own `start_server` path
//! (same RAM admission) and stops only models a paired Mac started.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use libp2p::PeerId;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::http;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "op")]
pub enum StatusRequest {
    Status,
    StartModel,
    StopModel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub name: String,
    pub app_version: String,
    pub model_id: Option<String>,
    pub model_running: bool,
    pub n_ctx: Option<u64>,
    /// A paired Mac's request is in flight (admission).
    pub busy: bool,
    pub queue_len: usize,
    pub on_battery: bool,
    pub can_start_model: bool,
    /// The share side's wire protocol ([`crate::proto::PROTO_VERSION`]); absent = 1 (M5).
    #[serde(default = "first_proto")]
    pub proto: u32,
    /// Oldest use-side protocol the share side accepts; absent = 1.
    #[serde(default = "first_proto")]
    pub min_proto: u32,
    /// The big Mac's model is working on a request that did not come through the
    /// forwarder ("busy with its own work", M5): llama-server's requests (processing +
    /// deferred, from `/metrics`, else busy `/slots`) exceed the forwarded requests that
    /// hold an upstream connection.
    #[serde(default)]
    pub local_busy: bool,
}

fn first_proto() -> u32 {
    1
}

impl Default for Status {
    fn default() -> Self {
        Self {
            name: String::new(),
            app_version: String::new(),
            model_id: None,
            model_running: false,
            n_ctx: None,
            busy: false,
            queue_len: 0,
            on_battery: false,
            can_start_model: false,
            proto: crate::proto::PROTO_VERSION,
            min_proto: crate::proto::MIN_PEER_PROTO,
            local_busy: false,
        }
    }
}

/// Whether two sides can talk (M5): each side's protocol must reach the other's minimum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compat {
    Ok,
    /// The other Mac (share side) is too old for this one.
    PeerTooOld,
    /// This Mac is too old for the other one.
    SelfTooOld,
}

impl Status {
    pub fn compat(&self, ours: crate::proto::Versions) -> Compat {
        if self.proto < ours.min_peer {
            Compat::PeerTooOld
        } else if ours.proto < self.min_proto {
            Compat::SelfTooOld
        } else {
            Compat::Ok
        }
    }
}

/// Refusal code for a protocol mismatch (status `Refused.code` and the 426 error code).
pub const VERSION_MISMATCH: &str = "version_mismatch";

/// The share side's refusal text for a too-old use side.
pub fn too_old_message(share_name: &str, share_app: &str) -> String {
    format!(
        "{share_name} runs Slipstream {share_app}, which needs Slipstream {} or newer on this Mac. Update this Mac.",
        crate::proto::PROTO_FIRST_APP
    )
}

/// Status request on the wire: `{"op":"status"|"start_model"|"stop_model", "proto":N,
/// "app_version":"x"}`. Pre-M5 senders omit the version fields.
pub fn encode_request(req: StatusRequest, versions: crate::proto::Versions) -> Vec<u8> {
    let mut v = serde_json::to_value(req).expect("static enum");
    if let Some(m) = v.as_object_mut() {
        m.insert("proto".into(), versions.proto.into());
        m.insert("app_version".into(), env!("CARGO_PKG_VERSION").into());
    }
    serde_json::to_vec(&v).expect("json")
}

/// Parse a status request (with or without version fields). Returns the peer's protocol
/// version if it sent one.
pub fn decode_request(bytes: &[u8]) -> io::Result<(StatusRequest, Option<u32>)> {
    let v: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let req = match v.get("op").and_then(|o| o.as_str()) {
        Some("status") => StatusRequest::Status,
        Some("start_model") => StatusRequest::StartModel,
        Some("stop_model") => StatusRequest::StopModel,
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown status op")),
    };
    let proto = v.get("proto").and_then(|p| p.as_u64()).map(|p| p.min(u32::MAX as u64) as u32);
    Ok((req, proto))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "result")]
pub enum StatusReply {
    Status(Status),
    Refused { code: String, message: String },
}

/// Future returned by [`ModelControl`] calls.
pub type ReplyFuture = Pin<Box<dyn Future<Output = StatusReply> + Send + 'static>>;

/// The share side's model lifecycle, provided by the app (v0.6 M4). The headless share
/// passes none and refuses `StartModel` / `StopModel`.
pub trait ModelControl: Send + Sync + 'static {
    /// Whether `peer` may start / stop the model right now ("Paired Macs may start and
    /// stop the model"). Reported as `Status::can_start_model`.
    fn allowed(&self, peer: &PeerId) -> bool;
    /// Start the model (through the app's RAM admission). Answer with a status or a
    /// refusal; the model may still be loading when this returns.
    fn start(&self, peer: PeerId) -> ReplyFuture;
    /// Stop the model — only one a paired Mac started, and only with no local request
    /// in flight.
    fn stop(&self, peer: PeerId) -> ReplyFuture;
}

pub const HEADLESS_REFUSAL: &str =
    "Starting or stopping the model remotely is not supported by the headless share (slipstream-node share forwards to an already running llama-server)";

/// Small `GET` against the local llama-server (Connection: close, bounded, 2 s).
pub async fn upstream_get(upstream: SocketAddr, path: &str) -> io::Result<(u16, Vec<u8>)> {
    let fut = async {
        let mut sock = TcpStream::connect(upstream).await?;
        sock.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {upstream}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
        let mut raw = Vec::new();
        (&mut sock).take(4 * 1024 * 1024).read_to_end(&mut raw).await?;
        let end = http::find_head_end(&raw)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no HTTP head"))?;
        let head = http::parse_response_head(&raw[..end])?;
        let body = if head.is_chunked() {
            http::dechunk_strict(&raw[end..], 4 * 1024 * 1024).unwrap_or_default()
        } else {
            raw[end..].to_vec()
        };
        Ok((head.status, body))
    };
    tokio::time::timeout(Duration::from_secs(2), fut)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "upstream status timed out"))?
}

fn on_battery() -> bool {
    std::process::Command::new("/usr/bin/pmset")
        .args(["-g", "ps"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("'Battery Power'"))
        .unwrap_or(false)
}

/// Requests llama-server is working on or holding: `requests_processing +
/// requests_deferred` from `/metrics` (the app starts it with `--metrics`), else the number
/// of busy slots in `/slots` (`is_processing`, or the Slipstream `phase`). `None` when the
/// server answers neither.
pub async fn upstream_requests(upstream: SocketAddr) -> Option<u64> {
    if let Ok((200, body)) = upstream_get(upstream, "/metrics").await {
        if let Some(n) = parse_metrics_requests(&String::from_utf8_lossy(&body)) {
            return Some(n);
        }
    }
    match upstream_get(upstream, "/slots").await {
        Ok((200, body)) => busy_slots(&body),
        _ => None,
    }
}

/// `llamacpp:requests_processing` + `llamacpp:requests_deferred`; `None` if neither line exists.
pub fn parse_metrics_requests(text: &str) -> Option<u64> {
    let mut seen = false;
    let mut n = 0f64;
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(k), Some(v)) = (it.next(), it.next()) else { continue };
        if k == "llamacpp:requests_processing" || k == "llamacpp:requests_deferred" {
            if let Ok(x) = v.parse::<f64>() {
                seen = true;
                n += x.max(0.0);
            }
        }
    }
    seen.then_some(n.round() as u64)
}

/// Busy slots in a `/slots` body (`is_processing: true`, or phase prompt/generating).
pub fn busy_slots(body: &[u8]) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let slots = v.as_array()?;
    Some(
        slots
            .iter()
            .filter(|s| {
                s.get("is_processing").and_then(|b| b.as_bool()).unwrap_or(false)
                    || matches!(s.get("phase").and_then(|p| p.as_str()), Some("prompt" | "generating"))
            })
            .count() as u64,
    )
}

/// Requests on the share side's model that did not come through the forwarder
/// (`forwarded` = forwarded requests holding an upstream connection). The reliable signal
/// behind "busy with its own work" and StopModel's "no local request in flight" (M5).
/// `None` when llama-server cannot be asked.
pub async fn local_requests(upstream: SocketAddr, forwarded: usize) -> Option<u64> {
    upstream_requests(upstream)
        .await
        .map(|n| n.saturating_sub(forwarded as u64))
}

/// What the share side knows when it answers a status request.
#[derive(Debug, Clone, Copy)]
pub struct ShareView<'a> {
    pub name: &'a str,
    pub upstream: SocketAddr,
    /// The upstream listener is the expected process (M3); otherwise it is not asked at all.
    pub upstream_ok: bool,
    /// Admission: (paired requests in flight, queued).
    pub load: (usize, usize),
    /// Forwarded requests that hold an upstream connection right now.
    pub forwarded: usize,
    pub versions: crate::proto::Versions,
}

/// Collect the share side's status from its llama-server + admission load.
pub async fn collect(view: ShareView<'_>) -> Status {
    let ShareView { name, upstream, upstream_ok, load, forwarded, versions } = view;
    let health = if upstream_ok { upstream_get(upstream, "/health").await.ok() } else { None };
    let model_running = matches!(health, Some((200, _)));
    let mut status = Status {
        name: name.to_string(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        model_running,
        busy: load.0 > 0,
        queue_len: load.1,
        on_battery: tokio::task::spawn_blocking(on_battery)
            .await
            .unwrap_or(false),
        can_start_model: false,
        proto: versions.proto,
        min_proto: versions.min_peer,
        ..Status::default()
    };
    if model_running {
        status.local_busy = local_requests(upstream, forwarded).await.unwrap_or(0) > 0;
        if let Ok((200, body)) = upstream_get(upstream, "/v1/models").await {
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            status.model_id = v["data"][0]["id"].as_str().map(str::to_string);
        }
        if let Ok((200, body)) = upstream_get(upstream, "/props").await {
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            status.n_ctx = v["default_generation_settings"]["n_ctx"]
                .as_u64()
                .or_else(|| v["n_ctx"].as_u64());
        }
    }
    status
}

/// How long a collected status is reused (security review L4: a paired Mac polling status
/// cannot make this Mac run `pmset` and five llama-server requests per poll).
pub const STATUS_CACHE_TTL: Duration = Duration::from_secs(1);
/// Status streams one peer may have open at once (L4).
pub const MAX_STATUS_STREAMS_PER_PEER: usize = 2;

/// [`collect`] reused for [`STATUS_CACHE_TTL`]; concurrent callers wait for one collection.
/// The admission load (`busy`, `queue_len`) is always live.
#[derive(Default)]
pub struct StatusCache {
    inner: tokio::sync::Mutex<Option<(std::time::Instant, Status)>>,
    collections: std::sync::atomic::AtomicU64,
}

impl StatusCache {
    pub async fn get(&self, view: ShareView<'_>) -> Status {
        let mut slot = self.inner.lock().await;
        let mut s = match &*slot {
            Some((t, s)) if t.elapsed() < STATUS_CACHE_TTL => s.clone(),
            _ => {
                self.collections.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let s = collect(view).await;
                *slot = Some((std::time::Instant::now(), s.clone()));
                s
            }
        };
        s.busy = view.load.0 > 0;
        s.queue_len = view.load.1;
        s
    }

    /// How many times the upstream was actually asked (tests).
    pub fn collections(&self) -> u64 {
        self.collections.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Answer one request from `peer` (which sent protocol `peer_proto`). A peer below
/// `view.versions.min_peer` is refused with [`VERSION_MISMATCH`] (M5). Without `model`
/// (headless share) start/stop are refused; with it they go to the app and
/// `can_start_model` says whether `peer` may.
pub async fn answer(
    req: StatusRequest,
    peer_proto: Option<u32>,
    view: ShareView<'_>,
    peer: PeerId,
    model: Option<&Arc<dyn ModelControl>>,
    cache: &StatusCache,
) -> StatusReply {
    if crate::proto::peer_proto(peer_proto) < view.versions.min_peer {
        return StatusReply::Refused {
            code: VERSION_MISMATCH.into(),
            message: too_old_message(view.name, env!("CARGO_PKG_VERSION")),
        };
    }
    let allowed = model.is_some_and(|m| m.allowed(&peer));
    match (req, model) {
        (StatusRequest::Status, _) => {
            let mut s = cache.get(view).await;
            s.can_start_model = allowed;
            StatusReply::Status(s)
        }
        (StatusRequest::StartModel | StatusRequest::StopModel, None) => StatusReply::Refused {
            code: "not_supported_headless".into(),
            message: HEADLESS_REFUSAL.into(),
        },
        (_, Some(_)) if !allowed => StatusReply::Refused {
            code: "model_control_off".into(),
            message: "This Mac does not let paired Macs start or stop its model.".into(),
        },
        (StatusRequest::StartModel, Some(m)) => m.start(peer).await,
        (StatusRequest::StopModel, Some(m)) => m.stop(peer).await,
    }
}

/// Default friendly name: the Mac's ComputerName (sent only over the authenticated
/// channel, never announced).
pub fn default_name() -> String {
    std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Mac".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Versions;

    #[test]
    fn request_wire_with_and_without_versions() {
        let b = encode_request(StatusRequest::StopModel, Versions::default());
        let (r, p) = decode_request(&b).unwrap();
        assert_eq!((r, p), (StatusRequest::StopModel, Some(crate::proto::PROTO_VERSION)));
        // pre-M5 encoding (plain enum)
        let old = serde_json::to_vec(&StatusRequest::Status).unwrap();
        assert_eq!(decode_request(&old).unwrap(), (StatusRequest::Status, None));
        assert!(decode_request(br#"{"op":"format_disk"}"#).is_err());
    }

    #[test]
    fn compat_rules() {
        let ours = Versions { proto: 2, min_peer: 2 };
        let s = |proto, min_proto| Status { proto, min_proto, ..Status::default() };
        assert_eq!(s(2, 1).compat(ours), Compat::Ok);
        assert_eq!(s(1, 1).compat(ours), Compat::PeerTooOld);
        assert_eq!(s(3, 3).compat(ours), Compat::SelfTooOld);
        // a pre-M5 status (no proto fields) counts as version 1
        let old: Status = serde_json::from_str(
            r#"{"name":"S","app_version":"0.5.3","model_id":null,"model_running":true,"n_ctx":null,
                "busy":false,"queue_len":0,"on_battery":false,"can_start_model":false}"#,
        )
        .unwrap();
        assert_eq!((old.proto, old.min_proto, old.local_busy), (1, 1, false));
        assert_eq!(old.compat(Versions::default()), Compat::Ok);
    }

    /// L4: status is collected at most once per TTL, however often a peer asks.
    #[tokio::test]
    async fn status_cache_reuses_one_collection() {
        let mock = crate::mock_llama::start("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let cache = StatusCache::default();
        let view = |load| ShareView {
            name: "S",
            upstream: mock.addr,
            upstream_ok: true,
            load,
            forwarded: 0,
            versions: Versions::default(),
        };
        let first = cache.get(view((0, 0))).await;
        let polls = futures::future::join_all((0..20).map(|_| cache.get(view((1, 3))))).await;
        assert_eq!(cache.collections(), 1);
        assert!(polls.iter().all(|s| s.busy && s.queue_len == 3), "load must stay live");
        assert_eq!(polls[0].model_running, first.model_running);
        tokio::time::sleep(STATUS_CACHE_TTL + Duration::from_millis(50)).await;
        cache.get(view((0, 0))).await;
        assert_eq!(cache.collections(), 2);
    }

    #[test]
    fn metrics_and_slots_request_counts() {
        let m = "# HELP x\nllamacpp:requests_processing 1\nllamacpp:requests_deferred 2\nllamacpp:prompt_tokens_total 9\n";
        assert_eq!(parse_metrics_requests(m), Some(3));
        assert_eq!(parse_metrics_requests("llamacpp:prompt_tokens_total 1\n"), None);
        assert_eq!(busy_slots(br#"[{"id":0,"is_processing":true},{"id":1,"is_processing":false}]"#), Some(1));
        assert_eq!(busy_slots(br#"[{"id":0,"phase":"generating"}]"#), Some(1));
        assert_eq!(busy_slots(b"nope"), None);
    }
}
