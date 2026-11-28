//! Shared harness for the M1 forwarder tests: mock llama-server, share side, use side
//! (separate identities in temp dirs), optional UDP shaper in between.
//!
//! M2: both sides are *paired* — each side's trust holds the other's PeerId and static
//! X25519 key (what pairing writes into the trust store), and every stream uses the
//! authenticated SealedStream.

#![allow(dead_code)]

pub mod udp_shaper;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use futures::StreamExt;
use p2p_crypto::StreamRole;
use p2p_forward::admission::AdmissionLimits;
use p2p_forward::mock_llama::{self, MockLlama};
use p2p_forward::proto::{channel_context, open_channel, BodyKind, FrameType, ReqHead, OPENAI_PROTOCOL_V1};
use p2p_forward::{
    load_or_create_identity, load_or_create_node, start_share, start_use, transport_identity, LocalLimits,
    ShareConfig, ShareHandle, ShareLimits, StaticTrust, UseConfig, UseHandle,
};
use std::sync::Arc;
use p2p_libp2p::paired::{build_paired_swarm, Control, PairedConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub struct Rig {
    pub dir: tempfile::TempDir,
    pub share_node: p2p_crypto::NodeKeypair,
    pub use_node: p2p_crypto::NodeKeypair,
    /// The share side's trusted peers (the MacBook).
    pub share_trust: Arc<StaticTrust>,
    /// The use side's trusted peers (the big Mac).
    pub use_trust: Arc<StaticTrust>,
    pub mock: MockLlama,
    pub share: ShareHandle,
    pub use_side: UseHandle,
    pub shaper: Option<udp_shaper::UdpShaper>,
}

pub struct RigOptions {
    pub rtt: Option<Duration>,
    pub paired: PairedConfig,
    pub spare_stream: bool,
    pub admission: AdmissionLimits,
    pub share_limits: ShareLimits,
    pub token: Option<String>,
    pub local: LocalLimits,
}

impl Default for RigOptions {
    fn default() -> Self {
        Self {
            rtt: None,
            paired: PairedConfig::default(),
            spare_stream: true,
            admission: AdmissionLimits::default(),
            share_limits: ShareLimits::default(),
            token: None,
            local: LocalLimits::default(),
        }
    }
}

pub async fn rig(opts: RigOptions) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let mock = mock_llama::start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let use_node = load_or_create_node(&dir.path().join("macbook")).unwrap();
    let share_node = load_or_create_node(&dir.path().join("big-mac")).unwrap();
    let use_identity = transport_identity(&use_node);
    let share_identity = transport_identity(&share_node);
    let share_trust = StaticTrust::new([(use_identity.public().to_peer_id(), use_node.public_bytes())]);
    let use_trust = StaticTrust::new([(share_identity.public().to_peer_id(), share_node.public_bytes())]);
    let share = start_share(
        share_identity,
        ShareConfig {
            listen: "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
            upstream: mock.addr,
            trust: share_trust.clone(),
            node: share_node.clone(),
            paired: opts.paired.clone(),
            admission: opts.admission,
            limits: opts.share_limits,
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
            trust: use_trust.clone(),
            node: use_node.clone(),
            bind: "127.0.0.1:0".parse().unwrap(),
            paired: opts.paired,
            spare_stream: opts.spare_stream,
            token: opts.token,
            local: opts.local,
            discovery: None,
            verbose: false,
        },
    )
    .await
    .unwrap();
    Rig {
        dir,
        share_node,
        use_node,
        share_trust,
        use_trust,
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

/// A raw HTTP/1.1 request. A query string in `target` is also sent as
/// `X-Mock-Params` (the share side drops queries; the mock reads the header).
pub fn request(method: &str, target: &str, host: &str, extra: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut s = format!("{method} {target} HTTP/1.1\r\nHost: {host}\r\n");
    if let Some((_, q)) = target.split_once('?') {
        s.push_str(&format!("X-Mock-Params: {q}\r\n"));
    }
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

/// A "modified use side": a raw swarm with the rig's MacBook identity that talks the
/// wire protocol directly (skips every use-side check).
pub struct RawPeer {
    pub control: Control,
    pub local: libp2p::PeerId,
    pub peer: libp2p::PeerId,
    pub node: p2p_crypto::NodeKeypair,
    pub peer_key: [u8; 32],
}

pub async fn raw_peer(r: &Rig) -> RawPeer {
    let identity = load_or_create_identity(&r.dir.path().join("macbook")).unwrap();
    let local = identity.public().to_peer_id();
    let mut swarm = build_paired_swarm(identity, &PairedConfig::default());
    swarm.dial(r.share.dial_addr()).unwrap();
    let control = swarm.behaviour().stream.new_control();
    tokio::spawn(async move {
        loop {
            swarm.select_next_some().await;
        }
    });
    RawPeer {
        control,
        local,
        peer: r.share.peer_id,
        node: r.use_node.clone(),
        peer_key: r.share_node.public_bytes(),
    }
}

impl RawPeer {
    pub async fn channel(&self) -> (p2p_forward::proto::ChannelReader, p2p_forward::proto::ChannelWriter) {
        let mut control = self.control.clone();
        let stream = control.open_stream(self.peer, OPENAI_PROTOCOL_V1).await.unwrap();
        let ctx = channel_context(&self.local, &self.peer, &OPENAI_PROTOCOL_V1);
        open_channel(stream, StreamRole::Initiator, &ctx, &self.node, &self.peer_key)
            .await
            .unwrap()
    }

    /// Send one ReqHead (+ body frames), return the raw response bytes as text
    /// (`Err` if the stream broke before the response ended).
    pub async fn exchange(
        &self,
        method: &str,
        target: &str,
        headers: Vec<(String, String)>,
        body: BodyKind,
        body_frames: &[Vec<u8>],
    ) -> Result<String, std::io::Error> {
        let (mut rd, mut wr) = self.channel().await;
        let head = ReqHead { method: method.into(), target: target.into(), headers, body };
        wr.send_json(FrameType::ReqHead, &head).await?;
        for f in body_frames {
            if wr.send(FrameType::ReqBody, f).await.is_err() {
                break;
            }
        }
        let _ = wr.send(FrameType::ReqBody, &[]).await;
        read_response(&mut rd).await
    }
}

pub async fn read_response(rd: &mut p2p_forward::proto::ChannelReader) -> Result<String, std::io::Error> {
    let mut out = Vec::new();
    while let Some(f) = rd.next().await? {
        if f.kind == FrameType::RespBody && f.payload.is_empty() {
            break;
        }
        out.extend_from_slice(&f.payload);
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}
