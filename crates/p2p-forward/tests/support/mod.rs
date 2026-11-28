//! Shared harness for the M1 forwarder tests: mock llama-server, share side, use side
//! (separate identities in temp dirs), optional UDP shaper in between.

#![allow(dead_code)]

pub mod udp_shaper;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use p2p_forward::admission::AdmissionLimits;
use p2p_forward::mock_llama::{self, MockLlama};
use p2p_forward::{load_or_create_identity, start_share, start_use, ShareConfig, ShareHandle, UseConfig, UseHandle};
use p2p_libp2p::paired::PairedConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub struct Rig {
    pub dir: tempfile::TempDir,
    pub mock: MockLlama,
    pub share: ShareHandle,
    pub use_side: UseHandle,
    pub shaper: Option<udp_shaper::UdpShaper>,
}

pub struct RigOptions {
    pub rtt: Option<Duration>,
    pub paired: PairedConfig,
    pub spare_stream: bool,
}

impl Default for RigOptions {
    fn default() -> Self {
        Self {
            rtt: None,
            paired: PairedConfig::default(),
            spare_stream: true,
        }
    }
}

pub async fn rig(opts: RigOptions) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let mock = mock_llama::start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let use_identity = load_or_create_identity(&dir.path().join("macbook")).unwrap();
    let share = start_share(
        load_or_create_identity(&dir.path().join("big-mac")).unwrap(),
        ShareConfig {
            listen: "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
            upstream: mock.addr,
            allow: vec![use_identity.public().to_peer_id()],
            paired: opts.paired.clone(),
            admission: AdmissionLimits::default(),
            name: "Test Studio".into(),
            verbose: false,
        },
    )
    .await
    .unwrap();
    let (peer, shaper) = match opts.rtt {
        Some(rtt) => {
            let s = udp_shaper::start(udp_shaper::udp_target(&share.listen_addr), rtt)
                .await
                .unwrap();
            (udp_shaper::via(&share.dial_addr(), s.addr), Some(s))
        }
        None => (share.dial_addr(), None),
    };
    let use_side = start_use(
        use_identity,
        UseConfig {
            peer,
            bind: "127.0.0.1:0".parse().unwrap(),
            paired: opts.paired,
            spare_stream: opts.spare_stream,
            verbose: false,
        },
    )
    .await
    .unwrap();
    Rig {
        dir,
        mock,
        share,
        use_side,
        shaper,
    }
}

/// A complete raw HTTP response (until the server closes).
pub struct Raw {
    pub bytes: Vec<u8>,
    pub ttft: Duration,
}

impl Raw {
    pub fn head(&self) -> String {
        let end = p2p_forward::http::find_head_end(&self.bytes).unwrap_or(self.bytes.len());
        String::from_utf8_lossy(&self.bytes[..end]).into_owned()
    }
    pub fn status(&self) -> u16 {
        self.head()
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }
    pub fn body(&self) -> &[u8] {
        let end = p2p_forward::http::find_head_end(&self.bytes).unwrap_or(self.bytes.len());
        &self.bytes[end..]
    }
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(self.body()).into_owned()
    }
}

pub fn request(method: &str, target: &str, host: &str, extra: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut s = format!("{method} {target} HTTP/1.1\r\nHost: {host}\r\n");
    for (k, v) in extra {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() || method == "POST" {
        s.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    s.push_str("Connection: close\r\n\r\n");
    let mut v = s.into_bytes();
    v.extend_from_slice(body);
    v
}

pub async fn send(addr: SocketAddr, req: &[u8]) -> Raw {
    let t0 = Instant::now();
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.set_nodelay(true).unwrap();
    sock.write_all(req).await.unwrap();
    let mut bytes = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut ttft = None;
    loop {
        let n = match sock.read(&mut buf).await {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
        if ttft.is_none() && bytes.windows(6).any(|w| w == b"data: ") {
            ttft = Some(t0.elapsed());
        }
    }
    Raw {
        ttft: ttft.unwrap_or_else(|| t0.elapsed()),
        bytes,
    }
}

pub fn chat_body(variant: &str) -> Vec<u8> {
    let v = match variant {
        "plain" => serde_json::json!({"model":"slipstream","stream":true,"messages":[{"role":"user","content":"hi"}]}),
        "json" => serde_json::json!({"model":"slipstream","stream":true,"messages":[{"role":"user","content":"json"}],
            "response_format":{"type":"json_schema","json_schema":{"name":"a","strict":true,"schema":{"type":"object"}}}}),
        "tool" => serde_json::json!({"model":"slipstream","stream":true,"messages":[{"role":"user","content":"19+23"}],
            "tools":[{"type":"function","function":{"name":"add","parameters":{"type":"object"}}}],
            "tool_choice":{"type":"function","function":{"name":"add"}}}),
        _ => unreachable!(),
    };
    serde_json::to_vec(&v).unwrap()
}

pub fn p95(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * 0.95).round() as usize]
}
