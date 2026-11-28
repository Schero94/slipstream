//! Wire protocol of the allowlisted forwarder (plan §3 `proto.rs`).
//!
//! Protocols: [`OPENAI_PROTOCOL_V1`] (one QUIC stream per HTTP request) and
//! [`STATUS_PROTOCOL_V1`] (status RPC). Every stream starts with the
//! [`p2p_crypto::SealedStream`] handshake (32-byte ephemeral X25519 key each way; keys
//! derived from ephemeral + the paired static X25519 keys (M2) and bound to both PeerIds +
//! the protocol id); after that each sealed AEAD frame carries
//! exactly one protocol frame:
//!
//! ```text
//! [u8 type][u32 BE len][payload]
//! ```
//!
//! | type | direction | payload |
//! |---|---|---|
//! | `ReqHead` | use → share | JSON [`ReqHead`] (method, target, filtered headers, body framing) |
//! | `ReqBody` | use → share | raw request body bytes; **empty = end of body** |
//! | `RespHead` | share → use | raw HTTP/1.1 response head as sent by llama-server |
//! | `RespBody` | share → use | raw response bytes (chunk framing included); **empty = end** |
//! | `Error` | share → use | JSON [`WireError`] before any `RespHead` |
//! | `Cancel` | use → share | empty; the editor went away → drop the upstream connection |
//! | `StatusReq` / `StatusResp` | status protocol | JSON |
//! | `Unpair` | pair protocol (M2) | empty; sender removed the receiver from its trust store |
//!
//! A clean end of a direction is the sealed FIN frame; a stream that ends without FIN
//! (reset, crash, peer loss) is an error and, on the share side, a cancel.

use std::io;

use p2p_crypto::{
    CryptoError, FrameHeader, NodeKeypair, FrameOpener, FrameSealer, StreamHandshake, StreamRole,
    FRAME_HEADER_LEN, MAX_FRAME_PLAINTEXT,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt};

pub use p2p_libp2p::paired::{OPENAI_PROTOCOL_V1, STATUS_PROTOCOL_V1};

/// Protocol frame header: type + u32 length.
pub const PROTO_HEADER_LEN: usize = 5;
/// Largest payload of one protocol frame (one protocol frame per sealed frame).
pub const MAX_PAYLOAD: usize = MAX_FRAME_PLAINTEXT - PROTO_HEADER_LEN;
/// Body bytes per `ReqBody` / `RespBody` frame (SSE events are far smaller).
pub const BODY_CHUNK: usize = 16 * 1024;
/// Request body cap, enforced on **both** ends (plan §3: 32 MiB, streamed).
pub const MAX_BODY: u64 = 32 * 1024 * 1024;

/// Version of this wire protocol (v0.6 M5). Carried in every [`ReqHead`] and status
/// request (use → share) and in every [`crate::Status`] (share → use). A peer whose
/// version is below the other side's [`Versions::min_peer`] is refused with a clear
/// "update to [`PROTO_FIRST_APP`] or newer" message instead of failing obscurely. A peer
/// that does not send a version speaks version 1 (the first one; nothing older shipped).
pub const PROTO_VERSION: u32 = 1;
/// Oldest peer protocol this build talks to.
pub const MIN_PEER_PROTO: u32 = 1;
/// First Slipstream app version that speaks [`MIN_PEER_PROTO`] (shown in the message).
pub const PROTO_FIRST_APP: &str = "0.6";

/// What one side speaks and accepts (tests use other values to provoke a mismatch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Versions {
    /// Advertised protocol version.
    pub proto: u32,
    /// Peers below this are refused.
    pub min_peer: u32,
}

impl Default for Versions {
    fn default() -> Self {
        Self {
            proto: PROTO_VERSION,
            min_peer: MIN_PEER_PROTO,
        }
    }
}

/// Version a peer speaks when it did not say (the first protocol).
pub fn peer_proto(sent: Option<u32>) -> u32 {
    sent.unwrap_or(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    ReqHead = 1,
    ReqBody = 2,
    RespHead = 3,
    RespBody = 4,
    Error = 5,
    Cancel = 6,
    StatusReq = 7,
    StatusResp = 8,
    /// M2: "I removed you from my paired Macs" (pair protocol, authenticated channel).
    Unpair = 9,
}

impl FrameType {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::ReqHead,
            2 => Self::ReqBody,
            3 => Self::RespHead,
            4 => Self::RespBody,
            5 => Self::Error,
            6 => Self::Cancel,
            7 => Self::StatusReq,
            8 => Self::StatusResp,
            9 => Self::Unpair,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: FrameType,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(kind: FrameType, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            kind,
            payload: payload.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtoError {
    #[error("frame shorter than its header")]
    Short,
    #[error("unknown frame type {0}")]
    UnknownType(u8),
    #[error("frame length {declared} does not match payload {actual}")]
    Length { declared: usize, actual: usize },
    #[error("frame payload too large ({0} bytes)")]
    TooLarge(usize),
}

/// Append one encoded frame to `out`.
pub fn encode(kind: FrameType, payload: &[u8], out: &mut Vec<u8>) -> Result<(), ProtoError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(ProtoError::TooLarge(payload.len()));
    }
    out.push(kind as u8);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

/// Decode exactly one frame occupying all of `buf` (strict: no trailing bytes).
pub fn decode(buf: &[u8]) -> Result<Frame, ProtoError> {
    if buf.len() < PROTO_HEADER_LEN {
        return Err(ProtoError::Short);
    }
    let kind = FrameType::from_u8(buf[0]).ok_or(ProtoError::UnknownType(buf[0]))?;
    let declared = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if declared > MAX_PAYLOAD {
        return Err(ProtoError::TooLarge(declared));
    }
    let actual = buf.len() - PROTO_HEADER_LEN;
    if declared != actual {
        return Err(ProtoError::Length { declared, actual });
    }
    Ok(Frame {
        kind,
        payload: buf[PROTO_HEADER_LEN..].to_vec(),
    })
}

/// How the request body is framed (the share side re-frames it towards llama-server).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyKind {
    None,
    Length(u64),
    Chunked,
}

/// First frame of an OpenAI stream. Headers are already filtered by the use side and
/// are filtered again by the share side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReqHead {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: BodyKind,
    /// The use side's [`PROTO_VERSION`] (M5); absent = 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proto: Option<u32>,
}

/// Failure before any response head (the use side turns it into an HTTP response).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

fn crypto_io(e: CryptoError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

fn proto_io(e: ProtoError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

/// Sends protocol frames, one sealed frame each, flushing after every frame.
pub struct FrameWriter<W> {
    w: W,
    tx: FrameSealer,
    plain: Vec<u8>,
    sealed: Vec<u8>,
    finished: bool,
    sent: u64,
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    pub fn new(w: W, tx: FrameSealer) -> Self {
        Self {
            w,
            tx,
            plain: Vec::with_capacity(BODY_CHUNK + PROTO_HEADER_LEN),
            sealed: Vec::with_capacity(BODY_CHUNK + 64),
            finished: false,
            sent: 0,
        }
    }

    /// Protocol frames sent so far (0 = the peer has seen nothing yet).
    pub fn frames_sent(&self) -> u64 {
        self.sent
    }

    pub async fn send(&mut self, kind: FrameType, payload: &[u8]) -> io::Result<()> {
        self.plain.clear();
        encode(kind, payload, &mut self.plain).map_err(proto_io)?;
        self.sealed.clear();
        self.tx
            .seal_into(&self.plain, false, &mut self.sealed)
            .map_err(crypto_io)?;
        self.sent += 1;
        self.w.write_all(&self.sealed).await?;
        self.w.flush().await
    }

    /// Body bytes of any length, split into [`BODY_CHUNK`] frames.
    pub async fn send_body(&mut self, kind: FrameType, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let n = bytes.len().min(BODY_CHUNK);
            self.send(kind, &bytes[..n]).await?;
            bytes = &bytes[n..];
        }
        Ok(())
    }

    pub async fn send_json<T: Serialize>(&mut self, kind: FrameType, value: &T) -> io::Result<()> {
        let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
        self.send(kind, &bytes).await
    }

    /// Clean end of this direction: sealed FIN frame, then shut the write side.
    pub async fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        self.sealed.clear();
        self.tx
            .seal_into(&[], true, &mut self.sealed)
            .map_err(crypto_io)?;
        self.w.write_all(&self.sealed).await?;
        self.w.flush().await?;
        self.w.shutdown().await
    }
}

/// Receives protocol frames; `Ok(None)` = the peer's clean FIN.
pub struct FrameReader<R> {
    r: R,
    rx: FrameOpener,
    body: Vec<u8>,
    /// First header byte already read by [`Self::wait_readable`].
    stash: Option<u8>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(r: R, rx: FrameOpener) -> Self {
        Self {
            r,
            rx,
            body: Vec::with_capacity(BODY_CHUNK + 64),
            stash: None,
        }
    }

    /// Wait until the next frame starts arriving (reads and keeps its first byte).
    /// Cancel-safe — unlike [`Self::next`], dropping this future never loses bytes — so
    /// it can sit under a timeout. `Ok(false)` = the stream ended.
    pub async fn wait_readable(&mut self) -> io::Result<bool> {
        if self.stash.is_some() {
            return Ok(true);
        }
        let mut b = [0u8; 1];
        let n = self.r.read(&mut b).await?;
        if n == 0 {
            return Ok(false);
        }
        self.stash = Some(b[0]);
        Ok(true)
    }

    pub async fn next(&mut self) -> io::Result<Option<Frame>> {
        let mut header = [0u8; FRAME_HEADER_LEN];
        let start = match self.stash.take() {
            Some(b) => {
                header[0] = b;
                1
            }
            None => 0,
        };
        if let Err(e) = self.r.read_exact(&mut header[start..]).await {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stream ended without FIN",
                ));
            }
            return Err(e);
        }
        let parsed = FrameHeader::parse(&header).map_err(crypto_io)?;
        self.body.resize(parsed.ciphertext_len, 0);
        self.r.read_exact(&mut self.body).await?;
        let fin = self
            .rx
            .open_in_place(&header, &mut self.body)
            .map_err(crypto_io)?;
        if fin {
            if !self.body.is_empty() {
                return Err(proto_io(ProtoError::Length {
                    declared: 0,
                    actual: self.body.len(),
                }));
            }
            return Ok(None);
        }
        decode(&self.body).map(Some).map_err(proto_io)
    }
}

pub type StreamIo = Compat<libp2p::Stream>;
pub type ChannelReader = FrameReader<ReadHalf<StreamIo>>;
pub type ChannelWriter = FrameWriter<WriteHalf<StreamIo>>;

/// Binding context for the stream keys: both transport identities + protocol.
pub fn channel_context(
    initiator: &libp2p::PeerId,
    responder: &libp2p::PeerId,
    protocol: &libp2p::StreamProtocol,
) -> Vec<u8> {
    format!("{initiator}|{responder}|{protocol}").into_bytes()
}

/// Run the authenticated SealedStream handshake (v0.6 M2: ephemeral + paired static
/// X25519 keys, [`StreamHandshake::finish_authenticated`]) on a fresh libp2p stream and
/// split it into a frame reader and writer. `local` = this node's static key,
/// `peer_static` = the peer's static key pinned in the trust store. No plaintext and no
/// unauthenticated fallback: a peer without the pinned static secret derives other keys,
/// so its first frame fails.
pub async fn open_channel(
    stream: libp2p::Stream,
    role: StreamRole,
    context: &[u8],
    local: &NodeKeypair,
    peer_static: &[u8; 32],
) -> io::Result<(ChannelReader, ChannelWriter)> {
    let (mut r, mut w) = tokio::io::split(stream.compat());
    let hs = StreamHandshake::new();
    w.write_all(&hs.public()).await?;
    w.flush().await?;
    let mut peer = [0u8; 32];
    r.read_exact(&mut peer).await?;
    let sealed = hs
        .finish_authenticated(&peer, role, context, local, peer_static)
        .map_err(crypto_io)?;
    Ok((FrameReader::new(r, sealed.rx), FrameWriter::new(w, sealed.tx)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_strict_decoding() {
        for kind in [
            FrameType::ReqHead,
            FrameType::ReqBody,
            FrameType::RespHead,
            FrameType::RespBody,
            FrameType::Error,
            FrameType::Cancel,
            FrameType::StatusReq,
            FrameType::StatusResp,
            FrameType::Unpair,
        ] {
            let mut out = Vec::new();
            encode(kind, b"payload", &mut out).unwrap();
            assert_eq!(decode(&out).unwrap(), Frame::new(kind, b"payload".to_vec()));
            let mut empty = Vec::new();
            encode(kind, &[], &mut empty).unwrap();
            assert_eq!(decode(&empty).unwrap().payload, b"");
        }
        assert_eq!(decode(&[1, 0, 0]), Err(ProtoError::Short));
        assert_eq!(decode(&[10, 0, 0, 0, 0]), Err(ProtoError::UnknownType(10)));
        assert_eq!(decode(&[0, 0, 0, 0, 0]), Err(ProtoError::UnknownType(0)));
        assert!(matches!(
            decode(&[2, 0, 0, 0, 2, b'x']),
            Err(ProtoError::Length { .. })
        ));
        assert!(matches!(
            decode(&[2, 0, 0, 0, 0, b'x']),
            Err(ProtoError::Length { .. })
        ));
        assert!(matches!(
            decode(&[2, 0xff, 0xff, 0xff, 0xff]),
            Err(ProtoError::TooLarge(_))
        ));
        let mut out = Vec::new();
        assert!(encode(FrameType::ReqBody, &vec![0; MAX_PAYLOAD + 1], &mut out).is_err());
        encode(FrameType::ReqBody, &vec![0; MAX_PAYLOAD], &mut out).unwrap();
        assert!(out.len() <= MAX_FRAME_PLAINTEXT);
    }

    /// Cheap deterministic fuzz of the decoder: it must never panic.
    #[test]
    fn decoder_never_panics_on_garbage() {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for len in 0..2000usize {
            let mut buf = Vec::with_capacity(len % 64);
            for _ in 0..(len % 64) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                buf.push(x as u8);
            }
            let _ = decode(&buf);
        }
    }

    /// `wait_readable` is cancel-safe under a timeout and `next` uses the stashed byte.
    #[tokio::test]
    async fn wait_readable_then_next() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let hs_a = StreamHandshake::new();
        let hs_b = StreamHandshake::new();
        let (pa, pb) = (hs_a.public(), hs_b.public());
        let sa = hs_a.finish(&pb, StreamRole::Initiator, b"ctx").unwrap();
        let sb = hs_b.finish(&pa, StreamRole::Responder, b"ctx").unwrap();
        let mut w = FrameWriter::new(a, sa.tx);
        let mut r = FrameReader::new(b, sb.rx);
        // nothing sent: the timeout fires, no bytes lost
        assert!(tokio::time::timeout(std::time::Duration::from_millis(20), r.wait_readable())
            .await
            .is_err());
        w.send(FrameType::ReqHead, b"hello").await.unwrap();
        assert!(r.wait_readable().await.unwrap());
        assert!(r.wait_readable().await.unwrap(), "idempotent");
        assert_eq!(r.next().await.unwrap().unwrap(), Frame::new(FrameType::ReqHead, b"hello".to_vec()));
        w.send(FrameType::ReqBody, b"x").await.unwrap();
        assert_eq!(r.next().await.unwrap().unwrap().payload, b"x");
    }

    #[test]
    fn req_head_json_roundtrip() {
        let head = ReqHead {
            method: "POST".into(),
            target: "/v1/chat/completions".into(),
            headers: vec![("content-type".into(), "application/json".into())],
            body: BodyKind::Length(12),
            proto: Some(PROTO_VERSION),
        };
        let json = serde_json::to_vec(&head).unwrap();
        assert_eq!(serde_json::from_slice::<ReqHead>(&json).unwrap(), head);
        // a pre-M5 head (no "proto") still parses and counts as version 1
        let old = br#"{"method":"GET","target":"/health","headers":[],"body":"none"}"#;
        let h: ReqHead = serde_json::from_slice(old).unwrap();
        assert_eq!(peer_proto(h.proto), 1);
    }
}
