//! Status RPC over `/slipstream/status/1` (plan §3 `status.rs`).
//!
//! The use side sends one `StatusReq` frame (JSON [`StatusRequest`]), the share side
//! answers with one `StatusResp` frame (JSON [`StatusReply`]) and FIN. `StartModel` /
//! `StopModel` are answered with a clear refusal in the headless share (the app-side
//! implementation through `start_server` + RAM admission is M4).

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub name: String,
    pub app_version: String,
    pub model_id: Option<String>,
    pub model_running: bool,
    pub n_ctx: Option<u64>,
    pub busy: bool,
    pub queue_len: usize,
    pub on_battery: bool,
    pub can_start_model: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "result")]
pub enum StatusReply {
    Status(Status),
    Refused { code: String, message: String },
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

/// Collect the share side's status from its llama-server + admission load.
pub async fn collect(name: &str, upstream: SocketAddr, load: (usize, usize)) -> Status {
    let health = upstream_get(upstream, "/health").await.ok();
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
        ..Status::default()
    };
    if model_running {
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

/// Answer one request (headless share).
pub async fn answer(
    req: StatusRequest,
    name: &str,
    upstream: SocketAddr,
    load: (usize, usize),
) -> StatusReply {
    match req {
        StatusRequest::Status => StatusReply::Status(collect(name, upstream, load).await),
        StatusRequest::StartModel | StatusRequest::StopModel => StatusReply::Refused {
            code: "not_supported_headless".into(),
            message: HEADLESS_REFUSAL.into(),
        },
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
