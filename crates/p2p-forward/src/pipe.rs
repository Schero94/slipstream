//! Byte pipes between a TCP socket and one libp2p stream: raw, or sealed with
//! [`p2p_crypto::SealedStream`] (per-stream ephemeral X25519, framed ChaCha20-Poly1305).

use std::io;

use p2p_crypto::{
    CryptoError, FrameHeader, FrameOpener, FrameSealer, StreamHandshake, StreamRole,
    FRAME_HEADER_LEN,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::compat::FuturesAsyncReadCompatExt;

/// Read size per sealed frame. SSE events are ~100–300 B, so in practice one frame
/// per upstream write; bulk bodies use up to this size.
pub const PIPE_CHUNK: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeMode {
    Raw,
    Sealed,
}

impl std::str::FromStr for PipeMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "raw" => Ok(Self::Raw),
            "sealed" => Ok(Self::Sealed),
            other => Err(format!("unknown pipe mode {other} (raw|sealed)")),
        }
    }
}

fn crypto_io(e: CryptoError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

/// Binding context for the stream keys: both transport identities + protocol.
pub fn stream_context(initiator: &libp2p::PeerId, responder: &libp2p::PeerId) -> Vec<u8> {
    format!(
        "{initiator}|{responder}|{}",
        p2p_libp2p::paired::PIPE_PROTOCOL_V0
    )
    .into_bytes()
}

/// Pipe `tcp` ⇄ `stream` until both directions are closed.
pub async fn pipe(
    tcp: TcpStream,
    stream: libp2p::Stream,
    mode: PipeMode,
    role: StreamRole,
    context: &[u8],
) -> io::Result<(u64, u64)> {
    tcp.set_nodelay(true)?;
    let quic = stream.compat();
    match mode {
        PipeMode::Raw => {
            let (mut tcp, mut quic) = (tcp, quic);
            tokio::io::copy_bidirectional(&mut tcp, &mut quic).await
        }
        PipeMode::Sealed => {
            let (quic_r, mut quic_w) = tokio::io::split(quic);
            let mut quic_r = quic_r;
            let (sealer, opener) = handshake(&mut quic_r, &mut quic_w, role, context).await?;
            let (tcp_r, tcp_w) = tcp.into_split();
            let up = seal_pump(tcp_r, quic_w, sealer);
            let down = open_pump(quic_r, tcp_w, opener);
            let (a, b) = tokio::try_join!(up, down)?;
            Ok((a, b))
        }
    }
}

/// Exchange ephemeral keys (each side writes its key, then reads the peer's).
pub async fn handshake<R, W>(
    r: &mut R,
    w: &mut W,
    role: StreamRole,
    context: &[u8],
) -> io::Result<(FrameSealer, FrameOpener)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let hs = StreamHandshake::new();
    w.write_all(&hs.public()).await?;
    w.flush().await?;
    let mut peer = [0u8; 32];
    r.read_exact(&mut peer).await?;
    let sealed = hs.finish(&peer, role, context).map_err(crypto_io)?;
    Ok((sealed.tx, sealed.rx))
}

/// Plaintext reader → sealed frames on `w`; EOF sends an empty FIN frame and shuts `w`.
pub async fn seal_pump<R, W>(mut r: R, mut w: W, mut tx: FrameSealer) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; PIPE_CHUNK];
    let mut out = Vec::with_capacity(PIPE_CHUNK + 64);
    let mut total = 0u64;
    loop {
        let n = r.read(&mut buf).await?;
        out.clear();
        tx.seal_into(&buf[..n], n == 0, &mut out)
            .map_err(crypto_io)?;
        w.write_all(&out).await?;
        w.flush().await?;
        if n == 0 {
            w.shutdown().await?;
            return Ok(total);
        }
        total += n as u64;
    }
}

/// Sealed frames from `r` → plaintext on `w`; requires a FIN frame before EOF.
pub async fn open_pump<R, W>(mut r: R, mut w: W, mut rx: FrameOpener) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut body = Vec::with_capacity(PIPE_CHUNK + 64);
    let mut total = 0u64;
    loop {
        let mut header = [0u8; FRAME_HEADER_LEN];
        if let Err(e) = r.read_exact(&mut header).await {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                rx.finish().map_err(crypto_io)?; // truncated without FIN → error
            }
            return Err(e);
        }
        let parsed = FrameHeader::parse(&header).map_err(crypto_io)?;
        body.resize(parsed.ciphertext_len, 0);
        r.read_exact(&mut body).await?;
        let fin = rx.open_in_place(&header, &mut body).map_err(crypto_io)?;
        if !body.is_empty() {
            w.write_all(&body).await?;
            w.flush().await?;
            total += body.len() as u64;
        }
        if fin {
            w.shutdown().await?;
            return Ok(total);
        }
    }
}
