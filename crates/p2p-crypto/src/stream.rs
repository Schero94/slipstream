//! `SealedStream` — framed AEAD for one long-lived byte stream (v0.6 M0 prototype).
//!
//! Layered *inside* the QUIC/TLS 1.3 stream between two paired Macs, so a TLS bug or
//! a misconfigured transport never exposes prompt bytes (docs/plans/2026-11-28-use-your-other-mac.md §3).
//!
//! # Handshake
//! Each side creates a fresh [`StreamHandshake`] per stream (ephemeral X25519) and sends
//! its 32-byte public key as the first bytes of the stream. Then
//! `HKDF-SHA256(salt = initiator_pub || responder_pub, ikm = DH, info = "slipstream-stream-v1" || context)`
//! expands to 64 bytes = two ChaCha20-Poly1305 keys, one per direction
//! (initiator→responder, responder→initiator). `context` should bind the transport
//! identities (e.g. both PeerIds and the protocol name); a mismatch makes every frame fail.
//!
//! The ephemeral DH alone is **unauthenticated**: peer authentication comes from the
//! QUIC TLS layer (pinned PeerIds) plus `context` binding. M2 can mix in the paired
//! static X25519 keys from the trust store.
//!
//! # Frames
//! `[u32 BE ciphertext_len][u8 flags][ciphertext || 16-byte tag]`, AAD = the 5 header
//! bytes. Nonce = 96-bit big-endian frame counter per direction (starts at 0, never
//! reused; the sealer refuses to wrap). Frames must be opened in order: a replayed,
//! dropped or reordered frame fails authentication. `FLAG_FIN` marks the last frame so
//! truncation is detectable ([`FrameOpener::finish`]).

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce, Tag};
use hkdf::Hkdf;
use rand::rngs::OsRng;
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey};
use zeroize::Zeroize;

use crate::error::CryptoError;

/// HKDF `info` prefix for stream keys. Bump when the layout changes.
pub const STREAM_HKDF_INFO: &[u8] = b"slipstream-stream-v1";
/// Frame header length: u32 ciphertext length + u8 flags.
pub const FRAME_HEADER_LEN: usize = 5;
/// Poly1305 tag length.
pub const TAG_LEN: usize = 16;
/// Largest plaintext per frame (64 KiB).
pub const MAX_FRAME_PLAINTEXT: usize = 64 * 1024;
/// Last frame of this direction.
pub const FLAG_FIN: u8 = 0x01;

/// Which end of the stream we are (the opener of the stream is the initiator).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamRole {
    Initiator,
    Responder,
}

/// One side of the per-stream ephemeral key exchange.
pub struct StreamHandshake {
    secret: EphemeralSecret,
    public: [u8; 32],
}

impl Default for StreamHandshake {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamHandshake {
    pub fn new() -> Self {
        let secret = EphemeralSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret).to_bytes();
        Self { secret, public }
    }

    /// Our ephemeral public key (send it as the first 32 bytes of the stream).
    pub fn public(&self) -> [u8; 32] {
        self.public
    }

    /// Finish with the peer's ephemeral public key; returns the two directional halves.
    pub fn finish(
        self,
        peer_public: &[u8; 32],
        role: StreamRole,
        context: &[u8],
    ) -> Result<SealedStream, CryptoError> {
        let shared = self.secret.diffie_hellman(&PublicKey::from(*peer_public));
        if !shared.was_contributory() {
            // low-order peer point → all-zero shared secret
            return Err(CryptoError::BadKey);
        }
        let (initiator_pub, responder_pub) = match role {
            StreamRole::Initiator => (self.public, *peer_public),
            StreamRole::Responder => (*peer_public, self.public),
        };
        let mut salt = [0u8; 64];
        salt[..32].copy_from_slice(&initiator_pub);
        salt[32..].copy_from_slice(&responder_pub);
        let mut info = Vec::with_capacity(STREAM_HKDF_INFO.len() + context.len());
        info.extend_from_slice(STREAM_HKDF_INFO);
        info.extend_from_slice(context);
        let hk = Hkdf::<Sha256>::new(Some(&salt), shared.as_bytes());
        let mut okm = [0u8; 64];
        hk.expand(&info, &mut okm).map_err(|_| CryptoError::Hkdf)?;
        let i2r = ChaCha20Poly1305::new_from_slice(&okm[..32]).map_err(|_| CryptoError::Hkdf)?;
        let r2i = ChaCha20Poly1305::new_from_slice(&okm[32..]).map_err(|_| CryptoError::Hkdf)?;
        okm.zeroize();
        let (tx, rx) = match role {
            StreamRole::Initiator => (i2r, r2i),
            StreamRole::Responder => (r2i, i2r),
        };
        Ok(SealedStream {
            tx: FrameSealer::new(tx),
            rx: FrameOpener::new(rx),
        })
    }
}

/// Both directions of one sealed stream; split them across tasks freely.
pub struct SealedStream {
    pub tx: FrameSealer,
    pub rx: FrameOpener,
}

fn nonce_for(counter: u128) -> Nonce {
    let bytes = counter.to_be_bytes(); // 16 bytes, use the low 12 (96 bits)
    *Nonce::from_slice(&bytes[4..])
}

const NONCE_LIMIT: u128 = 1u128 << 96;

/// Seals outgoing frames with a strictly increasing 96-bit counter nonce.
pub struct FrameSealer {
    cipher: ChaCha20Poly1305,
    counter: u128,
    finished: bool,
}

impl FrameSealer {
    fn new(cipher: ChaCha20Poly1305) -> Self {
        Self {
            cipher,
            counter: 0,
            finished: false,
        }
    }

    /// Frames sealed so far (= next nonce counter).
    pub fn frames_sealed(&self) -> u128 {
        self.counter
    }

    /// Append one frame (header + ciphertext + tag) for `plaintext` to `out`.
    pub fn seal_into(
        &mut self,
        plaintext: &[u8],
        fin: bool,
        out: &mut Vec<u8>,
    ) -> Result<(), CryptoError> {
        if self.finished || plaintext.len() > MAX_FRAME_PLAINTEXT || self.counter >= NONCE_LIMIT {
            return Err(CryptoError::Encrypt);
        }
        let ct_len = u32::try_from(plaintext.len() + TAG_LEN).map_err(|_| CryptoError::Encrypt)?;
        let mut header = [0u8; FRAME_HEADER_LEN];
        header[..4].copy_from_slice(&ct_len.to_be_bytes());
        header[4] = if fin { FLAG_FIN } else { 0 };
        let start = out.len();
        out.extend_from_slice(&header);
        out.extend_from_slice(plaintext);
        let nonce = nonce_for(self.counter);
        let tag = self
            .cipher
            .encrypt_in_place_detached(&nonce, &header, &mut out[start + FRAME_HEADER_LEN..])
            .map_err(|_| {
                out.truncate(start);
                CryptoError::Encrypt
            })?;
        out.extend_from_slice(&tag);
        self.counter += 1;
        self.finished = fin;
        Ok(())
    }

    /// Convenience: one frame as a fresh buffer.
    pub fn seal(&mut self, plaintext: &[u8], fin: bool) -> Result<Vec<u8>, CryptoError> {
        let mut out = Vec::with_capacity(FRAME_HEADER_LEN + plaintext.len() + TAG_LEN);
        self.seal_into(plaintext, fin, &mut out)?;
        Ok(out)
    }

    #[cfg(test)]
    fn set_counter(&mut self, counter: u128) {
        self.counter = counter;
    }
}

/// Parsed frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub ciphertext_len: usize,
    pub flags: u8,
}

impl FrameHeader {
    /// Parse and bound-check a header (rejects oversize / undersize / unknown flags
    /// before any allocation).
    pub fn parse(bytes: &[u8; FRAME_HEADER_LEN]) -> Result<Self, CryptoError> {
        let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let flags = bytes[4];
        if !(TAG_LEN..=MAX_FRAME_PLAINTEXT + TAG_LEN).contains(&len) || flags & !FLAG_FIN != 0 {
            return Err(CryptoError::Decrypt);
        }
        Ok(Self {
            ciphertext_len: len,
            flags,
        })
    }

    pub fn fin(&self) -> bool {
        self.flags & FLAG_FIN != 0
    }
}

/// Opens incoming frames in order; any tamper, replay, reorder or drop fails.
pub struct FrameOpener {
    cipher: ChaCha20Poly1305,
    counter: u128,
    finished: bool,
    poisoned: bool,
}

impl FrameOpener {
    fn new(cipher: ChaCha20Poly1305) -> Self {
        Self {
            cipher,
            counter: 0,
            finished: false,
            poisoned: false,
        }
    }

    /// Decrypt `body` (ciphertext + tag) in place; on success `body` holds the plaintext.
    /// Returns whether this was the FIN frame. After any failure the opener is poisoned.
    pub fn open_in_place(
        &mut self,
        header: &[u8; FRAME_HEADER_LEN],
        body: &mut Vec<u8>,
    ) -> Result<bool, CryptoError> {
        let result = self.open_inner(header, body);
        if result.is_err() {
            self.poisoned = true;
            body.clear();
        }
        result
    }

    fn open_inner(
        &mut self,
        header: &[u8; FRAME_HEADER_LEN],
        body: &mut Vec<u8>,
    ) -> Result<bool, CryptoError> {
        if self.poisoned || self.finished || self.counter >= NONCE_LIMIT {
            return Err(CryptoError::Decrypt);
        }
        let parsed = FrameHeader::parse(header)?;
        if body.len() != parsed.ciphertext_len {
            return Err(CryptoError::Decrypt);
        }
        let split = body.len() - TAG_LEN;
        let tag = Tag::clone_from_slice(&body[split..]);
        body.truncate(split);
        let nonce = nonce_for(self.counter);
        self.cipher
            .decrypt_in_place_detached(&nonce, header, body, &tag)
            .map_err(|_| CryptoError::Decrypt)?;
        self.counter += 1;
        self.finished = parsed.fin();
        Ok(self.finished)
    }

    /// Open one complete frame (header + body) from a buffer.
    pub fn open(&mut self, frame: &[u8]) -> Result<(Vec<u8>, bool), CryptoError> {
        if frame.len() < FRAME_HEADER_LEN {
            self.poisoned = true;
            return Err(CryptoError::Decrypt);
        }
        let mut header = [0u8; FRAME_HEADER_LEN];
        header.copy_from_slice(&frame[..FRAME_HEADER_LEN]);
        let mut body = frame[FRAME_HEADER_LEN..].to_vec();
        let fin = self.open_in_place(&header, &mut body)?;
        Ok((body, fin))
    }

    /// Call at end of the underlying stream: errors unless a FIN frame was opened
    /// (detects truncation by an on-path attacker or a crashed peer).
    pub fn finish(&self) -> Result<(), CryptoError> {
        if self.finished && !self.poisoned {
            Ok(())
        } else {
            Err(CryptoError::Decrypt)
        }
    }

    pub fn frames_opened(&self) -> u128 {
        self.counter
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn pair(ctx_i: &[u8], ctx_r: &[u8]) -> (SealedStream, SealedStream) {
        let i = StreamHandshake::new();
        let r = StreamHandshake::new();
        let (ip, rp) = (i.public(), r.public());
        (
            i.finish(&rp, StreamRole::Initiator, ctx_i).unwrap(),
            r.finish(&ip, StreamRole::Responder, ctx_r).unwrap(),
        )
    }

    #[test]
    fn roundtrip_both_directions_with_fin() {
        let (mut a, mut b) = pair(b"ctx", b"ctx");
        for i in 0..100u32 {
            let msg = format!("data: {{\"token\":{i}}}\n\n");
            let f = a.tx.seal(msg.as_bytes(), i == 99).unwrap();
            let (pt, fin) = b.rx.open(&f).unwrap();
            assert_eq!(pt, msg.as_bytes());
            assert_eq!(fin, i == 99);
        }
        b.rx.finish().unwrap();
        let f = b.tx.seal(b"POST /v1/chat/completions", true).unwrap();
        assert_eq!(a.rx.open(&f).unwrap().0, b"POST /v1/chat/completions");
        a.rx.finish().unwrap();
        // empty and max-size frames
        let (mut a, mut b) = pair(b"", b"");
        let big = vec![0xabu8; MAX_FRAME_PLAINTEXT];
        assert_eq!(b.rx.open(&a.tx.seal(&[], false).unwrap()).unwrap().0, b"");
        assert_eq!(b.rx.open(&a.tx.seal(&big, false).unwrap()).unwrap().0, big);
        assert!(a
            .tx
            .seal(&vec![0u8; MAX_FRAME_PLAINTEXT + 1], false)
            .is_err());
    }

    #[test]
    fn no_nonce_reuse_counters_and_direction_keys() {
        let (mut a, mut b) = pair(b"ctx", b"ctx");
        // Same plaintext sealed repeatedly must never give the same ciphertext
        // (unique nonce per frame), and the two directions use different keys.
        let mut seen = HashSet::new();
        for n in 0..10_000u128 {
            assert_eq!(a.tx.frames_sealed(), n);
            assert!(seen.insert(a.tx.seal(b"same", false).unwrap()));
        }
        for _ in 0..10_000 {
            assert!(seen.insert(b.tx.seal(b"same", false).unwrap()));
        }
        // nonce encoding is injective over the 96-bit space edges
        let nonces: HashSet<_> = [0u128, 1, 1 << 64, (1 << 96) - 1]
            .iter()
            .map(|c| nonce_for(*c).to_vec())
            .collect();
        assert_eq!(nonces.len(), 4);
        // two handshakes never share keys
        let (mut c, _) = pair(b"ctx", b"ctx");
        assert!(!seen.contains(&c.tx.seal(b"same", false).unwrap()));
    }

    #[test]
    fn sealer_refuses_counter_wrap() {
        let (mut a, _) = pair(b"", b"");
        a.tx.set_counter(NONCE_LIMIT - 1);
        a.tx.seal(b"last", false).unwrap();
        assert_eq!(a.tx.seal(b"wrap", false).unwrap_err(), CryptoError::Encrypt);
    }

    #[test]
    fn tamper_detection_ciphertext_tag_header() {
        let (mut a, _) = pair(b"ctx", b"ctx");
        let frame = a.tx.seal(b"data: secret prompt\n\n", false).unwrap();
        for pos in 0..frame.len() {
            for bit in [0x01u8, 0x80] {
                let (mut a2, mut b2) = pair(b"ctx", b"ctx");
                let mut f = a2.tx.seal(b"data: secret prompt\n\n", false).unwrap();
                f[pos] ^= bit;
                assert!(b2.rx.open(&f).is_err(), "flip at {pos} not detected");
                // poisoned afterwards, even for a valid next frame
                let ok = a2.tx.seal(b"x", false).unwrap();
                assert!(b2.rx.open(&ok).is_err());
            }
        }
        // FIN flag flip (header is AAD) is detected
        let (mut a3, mut b3) = pair(b"ctx", b"ctx");
        let mut f = a3.tx.seal(b"not last", false).unwrap();
        f[4] = FLAG_FIN;
        assert!(b3.rx.open(&f).is_err());
    }

    #[test]
    fn replay_reorder_drop_and_truncation_fail() {
        let (mut a, mut b) = pair(b"ctx", b"ctx");
        let f0 = a.tx.seal(b"zero", false).unwrap();
        let f1 = a.tx.seal(b"one", false).unwrap();
        let f2 = a.tx.seal(b"two", true).unwrap();
        b.rx.open(&f0).unwrap();
        assert!(b.rx.open(&f0).is_err(), "replay accepted");

        let (mut a, mut b) = pair(b"ctx", b"ctx");
        let _f0 = a.tx.seal(b"zero", false).unwrap();
        let f1b = a.tx.seal(b"one", false).unwrap();
        assert!(b.rx.open(&f1b).is_err(), "drop/reorder accepted");

        let (mut a, mut b) = pair(b"ctx", b"ctx");
        b.rx.open(&a.tx.seal(b"zero", false).unwrap()).unwrap();
        assert!(b.rx.finish().is_err(), "truncation without FIN accepted");
        let _ = (f1, f2);

        // frames from another stream are rejected
        let (mut x, _) = pair(b"ctx", b"ctx");
        let (_, mut y) = pair(b"ctx", b"ctx");
        assert!(y.rx.open(&x.tx.seal(b"cross", false).unwrap()).is_err());
    }

    #[test]
    fn context_mismatch_fails() {
        let (mut a, mut b) = pair(
            b"peerA|peerB|/slipstream/openai/1",
            b"peerA|peerX|/slipstream/openai/1",
        );
        assert!(b.rx.open(&a.tx.seal(b"hi", false).unwrap()).is_err());
    }

    #[test]
    fn low_order_point_rejected_and_bad_headers() {
        let hs = StreamHandshake::new();
        assert_eq!(
            hs.finish(&[0u8; 32], StreamRole::Initiator, b"").err(),
            Some(CryptoError::BadKey)
        );
        let too_big = ((MAX_FRAME_PLAINTEXT + TAG_LEN + 1) as u32).to_be_bytes();
        assert!(FrameHeader::parse(&[too_big[0], too_big[1], too_big[2], too_big[3], 0]).is_err());
        assert!(FrameHeader::parse(&[0, 0, 0, 15, 0]).is_err()); // shorter than a tag
        assert!(FrameHeader::parse(&[0, 0, 0, 16, 0x02]).is_err()); // unknown flag
        assert!(FrameHeader::parse(&[0, 0, 0, 16, FLAG_FIN]).unwrap().fin());
    }
}
