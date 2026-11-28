//! Measurement client + AEAD micro-benchmark for the M0 spike (`bench/forward/`).

use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use p2p_crypto::{StreamHandshake, StreamRole, FRAME_HEADER_LEN};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// One streamed HTTP request as seen by the client (the editor's view).
#[derive(Debug, Clone)]
pub struct Sample {
    /// Request start (before TCP connect) → first response *body* byte.
    pub ttft_us: f64,
    /// Request start → connection closed by the server.
    pub total_us: f64,
    /// SSE events (`data: ` lines) seen.
    pub events: usize,
    /// (events − 1) / (last event arrival − first event arrival).
    pub event_rate: f64,
    /// SHA-256 of the raw body bytes (after the HTTP head, before de-chunking).
    pub body_sha256: String,
    /// SHA-256 of the concatenated generated text (OpenAI `delta.content` or
    /// llama `content`), which is stable for greedy runs even when the raw body holds
    /// timestamps.
    pub content_sha256: String,
    pub body_len: usize,
    pub status_line: String,
}

fn count_events(bytes: &[u8]) -> usize {
    bytes.windows(6).filter(|w| w == b"data: ").count()
}

/// Send `request` (a complete HTTP/1.1 request) to `addr` and read until close.
pub async fn run_request(addr: SocketAddr, request: &[u8]) -> io::Result<Sample> {
    let t0 = Instant::now();
    let mut sock = TcpStream::connect(addr).await?;
    sock.set_nodelay(true)?;
    sock.write_all(request).await?;
    let mut raw = Vec::with_capacity(256 * 1024);
    let mut chunk = vec![0u8; 64 * 1024];
    let mut head_end: Option<usize> = None;
    let mut ttft = None;
    let mut first_event = None;
    let mut last_event = None;
    let mut events = 0usize;
    loop {
        let n = sock.read(&mut chunk).await?;
        let now = t0.elapsed();
        if n == 0 {
            break;
        }
        let scan_from = raw.len().saturating_sub(5);
        raw.extend_from_slice(&chunk[..n]);
        if head_end.is_none() {
            if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                head_end = Some(pos + 4);
            }
        }
        if let Some(h) = head_end {
            if raw.len() > h && ttft.is_none() {
                ttft = Some(now);
            }
            let from = scan_from.max(h);
            let new_events = count_events(&raw[from..]);
            if new_events > 0 {
                events += new_events;
                first_event.get_or_insert(now);
                last_event = Some(now);
            }
        }
    }
    let total = t0.elapsed();
    let h = head_end.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no HTTP head"))?;
    let head = String::from_utf8_lossy(&raw[..h]).into_owned();
    let body = &raw[h..];
    let chunked = head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let decoded = if chunked {
        dechunk(body)
    } else {
        body.to_vec()
    };
    let span = match (first_event, last_event) {
        (Some(a), Some(b)) if b > a => (b - a).as_secs_f64(),
        _ => 0.0,
    };
    Ok(Sample {
        ttft_us: ttft.unwrap_or(total).as_secs_f64() * 1e6,
        total_us: total.as_secs_f64() * 1e6,
        events,
        event_rate: if span > 0.0 {
            (events.saturating_sub(1)) as f64 / span
        } else {
            0.0
        },
        body_sha256: hex::encode(Sha256::digest(body)),
        content_sha256: hex::encode(Sha256::digest(extract_content(&decoded).as_bytes())),
        body_len: body.len(),
        status_line: head.lines().next().unwrap_or("").to_string(),
    })
}

/// Minimal HTTP/1.1 chunked decoder (post-processing only).
pub fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    loop {
        let Some(line_end) = body.windows(2).position(|w| w == b"\r\n") else {
            break;
        };
        let size_str = String::from_utf8_lossy(&body[..line_end]);
        let size =
            usize::from_str_radix(size_str.split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
        body = &body[line_end + 2..];
        if size == 0 || body.len() < size {
            break;
        }
        out.extend_from_slice(&body[..size]);
        body = &body[(size + 2).min(body.len())..];
    }
    out
}

/// Concatenate generated text from SSE `data:` JSON lines.
pub fn extract_content(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let mut out = String::new();
    for line in text.lines() {
        let Some(json) = line.strip_prefix("data: ") else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
            continue;
        };
        if let Some(c) = v
            .pointer("/choices/0/delta/content")
            .and_then(|c| c.as_str())
        {
            out.push_str(c);
        } else if let Some(c) = v.get("content").and_then(|c| c.as_str()) {
            out.push_str(c);
        }
    }
    out
}

pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

#[derive(Debug, Clone)]
pub struct AeadResult {
    pub chunk_bytes: usize,
    pub seal_ns: f64,
    pub open_ns: f64,
}

/// Seal + open `iters` frames of `chunk_bytes` and report ns per frame each way.
pub fn aead_bench(chunk_bytes: usize, iters: usize) -> AeadResult {
    let i = StreamHandshake::new();
    let r = StreamHandshake::new();
    let (ip, rp) = (i.public(), r.public());
    let mut a = i.finish(&rp, StreamRole::Initiator, b"bench").unwrap();
    let mut b = r.finish(&ip, StreamRole::Responder, b"bench").unwrap();
    let payload = vec![0x5au8; chunk_bytes];
    let mut frames = Vec::with_capacity(iters);
    // warm-up
    for _ in 0..1000 {
        let f = a.tx.seal(&payload, false).unwrap();
        b.rx.open(&f).unwrap();
    }
    let mut out = Vec::with_capacity(chunk_bytes + 64);
    let t = Instant::now();
    for _ in 0..iters {
        out.clear();
        a.tx.seal_into(&payload, false, &mut out).unwrap();
        frames.push(out.clone());
    }
    let seal_total = t.elapsed();
    // subtract the clone cost measured separately
    let t = Instant::now();
    let mut sink = Vec::with_capacity(iters);
    for f in &frames {
        sink.push(f.clone());
    }
    let clone_total = t.elapsed();
    drop(sink);
    let mut body = Vec::with_capacity(chunk_bytes + 64);
    let t = Instant::now();
    for f in &frames {
        let mut header = [0u8; FRAME_HEADER_LEN];
        header.copy_from_slice(&f[..FRAME_HEADER_LEN]);
        body.clear();
        body.extend_from_slice(&f[FRAME_HEADER_LEN..]);
        b.rx.open_in_place(&header, &mut body).unwrap();
    }
    let open_total = t.elapsed();
    let per = |d: Duration| d.as_secs_f64() * 1e9 / iters as f64;
    AeadResult {
        chunk_bytes,
        seal_ns: (per(seal_total) - per(clone_total)).max(0.0),
        open_ns: per(open_total),
    }
}

/// Same framing cost with ring's assembly ChaCha20-Poly1305 (M1 backend candidate):
/// seal + open of one `chunk_bytes` record with a counter nonce, ns per frame each way.
pub fn ring_aead_bench(chunk_bytes: usize, iters: usize) -> AeadResult {
    use ring::aead::{Aad, LessSafeKey, Nonce as RNonce, UnboundKey, CHACHA20_POLY1305};
    let key = [7u8; 32];
    let sealer = LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &key).unwrap());
    let opener = LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &key).unwrap());
    let nonce = |c: u64| {
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&c.to_be_bytes());
        RNonce::assume_unique_for_key(n)
    };
    let header = [0u8; FRAME_HEADER_LEN];
    let payload = vec![0x5au8; chunk_bytes];
    let mut frames: Vec<Vec<u8>> = Vec::with_capacity(iters);
    let mut buf = Vec::with_capacity(chunk_bytes + 32);
    let t = Instant::now();
    for c in 0..iters as u64 {
        buf.clear();
        buf.extend_from_slice(&payload);
        let tag = sealer
            .seal_in_place_separate_tag(nonce(c), Aad::from(header), &mut buf)
            .unwrap();
        buf.extend_from_slice(tag.as_ref());
        frames.push(buf.clone());
    }
    let seal_total = t.elapsed();
    let t = Instant::now();
    let mut sink = Vec::with_capacity(iters);
    for f in &frames {
        sink.push(f.clone());
    }
    let clone_total = t.elapsed();
    drop(sink);
    let t = Instant::now();
    for (c, f) in frames.iter_mut().enumerate() {
        opener
            .open_in_place(nonce(c as u64), Aad::from(header), f)
            .unwrap();
    }
    let open_total = t.elapsed();
    let per = |d: Duration| d.as_secs_f64() * 1e9 / iters as f64;
    AeadResult {
        chunk_bytes,
        seal_ns: (per(seal_total) - per(clone_total)).max(0.0),
        open_ns: per(open_total),
    }
}
