//! u32 big-endian length-prefixed framing over async streams.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::NetError;

/// Soft max for a single frame body (4 MiB). Enough for MVP job envelopes.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Write `body` as `[u32 BE length][body]`.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    body: &[u8],
) -> Result<(), NetError> {
    if body.len() > MAX_FRAME_BYTES {
        return Err(NetError::FrameTooLarge(body.len(), MAX_FRAME_BYTES));
    }
    let len = (body.len() as u32).to_be_bytes();
    writer.write_all(&len).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one length-prefixed frame body.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, NetError> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len == 0 || len as usize > MAX_FRAME_BYTES {
        return Err(NetError::BadFrameLength(len, MAX_FRAME_BYTES));
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body).await?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncWriteExt};

    #[tokio::test]
    async fn frame_roundtrip() {
        let (mut a, mut b) = duplex(1024);
        let payload = b"{\"type\":\"heartbeat\",\"seq\":1}";
        write_frame(&mut a, payload).await.unwrap();
        let got = read_frame(&mut b).await.unwrap();
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn rejects_oversized_write() {
        let (mut a, _b) = duplex(64);
        let huge = vec![0u8; MAX_FRAME_BYTES + 1];
        let err = write_frame(&mut a, &huge).await.unwrap_err();
        match err {
            NetError::FrameTooLarge(n, max) => {
                assert_eq!(n, MAX_FRAME_BYTES + 1);
                assert_eq!(max, MAX_FRAME_BYTES);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_zero_length_prefix() {
        let (mut a, mut b) = duplex(64);
        a.write_all(&0u32.to_be_bytes()).await.unwrap();
        a.flush().await.unwrap();
        let err = read_frame(&mut b).await.unwrap_err();
        // Display must stay actionable for multi-node logs.
        let display = err.to_string();
        assert!(
            display.contains("allowed 1..="),
            "display={display}"
        );
        match err {
            NetError::BadFrameLength(0, max) => assert_eq!(max, MAX_FRAME_BYTES),
            other => panic!("unexpected {other:?}"),
        }
    }
}
