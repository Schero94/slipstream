//! Pairing with a 6-digit one-time code: SPAKE2 + key confirmation (v0.6 M2).
//!
//! docs/plans/2026-11-28-use-your-other-mac.md §2. The sharing Mac ("server", SPAKE2
//! side B) shows a code; the Mac that wants to use it ("client", side A) types it in.
//!
//! - SPAKE2 over the Ed25519 group (`spake2` 0.4, RustCrypto): `idA` = client PeerId
//!   bytes, `idB` = server PeerId bytes, password = the 6 ASCII digits. Both identities
//!   are hashed into the session key, so a relay that terminates the transport under its
//!   own PeerId ends up with different keys on each side and fails key confirmation.
//! - An online attacker gets exactly one code guess per session (SPAKE2); the share side
//!   allows 3 failed sessions per window and 5 minutes (enforced by the caller).
//! - Key confirmation, one tag per side:
//!   `HMAC-SHA256(Kc, "slipstream-pair-v1" ‖ role ‖ PeerIdA ‖ PeerIdB ‖ X25519A ‖ X25519B
//!   ‖ name ‖ app_version ‖ may_start_model ‖ prev)`, where `Kc = HKDF(K, "confirm")`,
//!   `role` is `A`/`B` (so a tag cannot be reflected), variable fields are length-prefixed,
//!   `name`/`app_version`/`may_start_model` are the **sender's** claims and `prev` is the
//!   peer's confirmation tag (empty for B, B's tag for A — the second tag covers the whole
//!   exchange). Verification uses HMAC's constant-time compare.
//! - The claims ([`PairInfo`]) travel encrypted under K (ChaCha20-Poly1305, one key per
//!   direction derived from K, each key used for exactly one message).
//!
//! This module is pure (no I/O); `apps/p2p-node/src/pairing.rs` runs it over
//! `/slipstream/pair/1`.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Domain label of the pairing transcript (bump when the layout changes).
pub const PAIR_TRANSCRIPT_LABEL: &[u8] = b"slipstream-pair-v1";
/// Digits in a pairing code.
pub const CODE_DIGITS: usize = 6;
/// Upper bound for a claimed name / version (bytes) — both are display strings.
pub const MAX_CLAIM_LEN: usize = 128;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PakeError {
    #[error("the pairing code must be exactly 6 digits")]
    BadCode,
    #[error("malformed pairing message")]
    BadMessage,
    #[error("key confirmation failed (wrong code, or someone is in between)")]
    Confirmation,
}

/// A one-time 6-digit pairing code. Uniform over 000000–999999 from the OS RNG.
#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct PairingCode(String);

impl PairingCode {
    pub fn generate() -> Self {
        let n: u32 = OsRng.gen_range(0..1_000_000);
        Self(format!("{n:06}"))
    }

    /// Accepts exactly six ASCII digits; spaces and dashes (`123 456`, `123-456`) are
    /// ignored so a code read aloud can be typed as heard.
    pub fn parse(s: &str) -> Result<Self, PakeError> {
        let digits: String = s.chars().filter(|c| *c != ' ' && *c != '-').collect();
        if digits.len() != CODE_DIGITS || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(PakeError::BadCode);
        }
        Ok(Self(digits))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingCode(******)")
    }
}

/// Which end of the pairing we are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PakeRole {
    /// SPAKE2 side A: the Mac that dials and types the code (uses the other Mac).
    Client,
    /// SPAKE2 side B: the Mac that shows the code (shares its model).
    Server,
}

impl PakeRole {
    fn label(self) -> u8 {
        match self {
            PakeRole::Client => b'A',
            PakeRole::Server => b'B',
        }
    }
}

/// What a side claims about itself; bound into its confirmation tag and sent encrypted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairInfo {
    pub name: String,
    pub app_version: String,
    /// Server: "you may start/stop my model". Client: always false (a MacBook that uses
    /// a model does not offer one through this pairing).
    pub may_start_model: bool,
}

/// Public inputs both sides must agree on.
#[derive(Debug, Clone, Copy)]
pub struct Transcript<'a> {
    pub peer_id_a: &'a [u8],
    pub peer_id_b: &'a [u8],
    pub x25519_a: [u8; 32],
    pub x25519_b: [u8; 32],
}

/// First half of SPAKE2.
pub struct PakeStart {
    inner: Spake2<Ed25519Group>,
    role: PakeRole,
}

impl PakeStart {
    /// Start as `role`; returns the state and the message to send (33 bytes).
    pub fn new(role: PakeRole, code: &PairingCode, peer_id_a: &[u8], peer_id_b: &[u8]) -> (Self, Vec<u8>) {
        let pw = Password::new(code.as_str().as_bytes());
        let (ida, idb) = (Identity::new(peer_id_a), Identity::new(peer_id_b));
        let (inner, msg) = match role {
            PakeRole::Client => Spake2::<Ed25519Group>::start_a(&pw, &ida, &idb),
            PakeRole::Server => Spake2::<Ed25519Group>::start_b(&pw, &ida, &idb),
        };
        (Self { inner, role }, msg)
    }

    /// Finish with the peer's SPAKE2 message. Never fails on a wrong code (SPAKE2 cannot
    /// tell); the confirmation tags do.
    pub fn finish(self, peer_msg: &[u8]) -> Result<SessionKey, PakeError> {
        let mut k = self.inner.finish(peer_msg).map_err(|_| PakeError::BadMessage)?;
        let hk = Hkdf::<Sha256>::new(Some(PAIR_TRANSCRIPT_LABEL), &k);
        k.zeroize();
        let mut out = SessionKey {
            role: self.role,
            confirm: [0; 32],
            a2b: [0; 32],
            b2a: [0; 32],
        };
        hk.expand(b"confirm", &mut out.confirm).map_err(|_| PakeError::BadMessage)?;
        hk.expand(b"info a2b", &mut out.a2b).map_err(|_| PakeError::BadMessage)?;
        hk.expand(b"info b2a", &mut out.b2a).map_err(|_| PakeError::BadMessage)?;
        Ok(out)
    }
}

/// Keys derived from the SPAKE2 session key K.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SessionKey {
    #[zeroize(skip)]
    role: PakeRole,
    confirm: [u8; 32],
    a2b: [u8; 32],
    b2a: [u8; 32],
}

type HmacSha256 = Hmac<Sha256>;

fn put_lp(mac: &mut HmacSha256, field: &[u8]) {
    mac.update(&(field.len() as u32).to_be_bytes());
    mac.update(field);
}

impl SessionKey {
    fn transcript_mac(&self, role: PakeRole, t: &Transcript<'_>, info: &PairInfo, prev: &[u8]) -> HmacSha256 {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.confirm).expect("any key length");
        mac.update(PAIR_TRANSCRIPT_LABEL);
        mac.update(&[role.label()]);
        put_lp(&mut mac, t.peer_id_a);
        put_lp(&mut mac, t.peer_id_b);
        mac.update(&t.x25519_a);
        mac.update(&t.x25519_b);
        put_lp(&mut mac, info.name.as_bytes());
        put_lp(&mut mac, info.app_version.as_bytes());
        mac.update(&[info.may_start_model as u8]);
        put_lp(&mut mac, prev);
        mac
    }

    /// Our confirmation tag (`info` = our own claims, `prev` = the peer's tag or empty).
    pub fn confirm(&self, t: &Transcript<'_>, info: &PairInfo, prev: &[u8]) -> [u8; 32] {
        self.transcript_mac(self.role, t, info, prev).finalize().into_bytes().into()
    }

    /// Check the peer's tag over the peer's claims (constant-time compare).
    pub fn verify_peer(&self, t: &Transcript<'_>, peer_info: &PairInfo, prev: &[u8], tag: &[u8]) -> Result<(), PakeError> {
        let peer_role = match self.role {
            PakeRole::Client => PakeRole::Server,
            PakeRole::Server => PakeRole::Client,
        };
        self.transcript_mac(peer_role, t, peer_info, prev)
            .verify_slice(tag)
            .map_err(|_| PakeError::Confirmation)
    }

    fn cipher(&self, sender: PakeRole) -> ChaCha20Poly1305 {
        let key = match sender {
            PakeRole::Client => &self.a2b,
            PakeRole::Server => &self.b2a,
        };
        ChaCha20Poly1305::new(Key::from_slice(key))
    }

    /// Encrypt our claims (nonce 0 of our direction's key).
    pub fn seal_info(&self, info: &PairInfo) -> Vec<u8> {
        let plain = serde_json::to_vec(info).expect("PairInfo serializes");
        self.cipher(self.role)
            .encrypt(Nonce::from_slice(&[0u8; 12]), Payload { msg: &plain, aad: PAIR_TRANSCRIPT_LABEL })
            .expect("chacha20poly1305 encrypt")
    }

    /// Decrypt the peer's claims; a wrong code fails here (AEAD) or at the tag.
    pub fn open_info(&self, ciphertext: &[u8]) -> Result<PairInfo, PakeError> {
        let peer = match self.role {
            PakeRole::Client => PakeRole::Server,
            PakeRole::Server => PakeRole::Client,
        };
        let plain = self
            .cipher(peer)
            .decrypt(Nonce::from_slice(&[0u8; 12]), Payload { msg: ciphertext, aad: PAIR_TRANSCRIPT_LABEL })
            .map_err(|_| PakeError::Confirmation)?;
        let info: PairInfo = serde_json::from_slice(&plain).map_err(|_| PakeError::BadMessage)?;
        if info.name.len() > MAX_CLAIM_LEN || info.app_version.len() > MAX_CLAIM_LEN {
            return Err(PakeError::BadMessage);
        }
        Ok(info)
    }

    /// The server's final acknowledgement (nonce 1 of the b2a key): "I stored you".
    pub fn seal_ack(&self) -> Vec<u8> {
        self.cipher(PakeRole::Server)
            .encrypt(Nonce::from_slice(&ack_nonce()), Payload { msg: b"paired", aad: PAIR_TRANSCRIPT_LABEL })
            .expect("chacha20poly1305 encrypt")
    }

    pub fn open_ack(&self, ciphertext: &[u8]) -> Result<(), PakeError> {
        let plain = self
            .cipher(PakeRole::Server)
            .decrypt(Nonce::from_slice(&ack_nonce()), Payload { msg: ciphertext, aad: PAIR_TRANSCRIPT_LABEL })
            .map_err(|_| PakeError::Confirmation)?;
        (plain == b"paired").then_some(()).ok_or(PakeError::Confirmation)
    }
}

fn ack_nonce() -> [u8; 12] {
    let mut n = [0u8; 12];
    n[11] = 1;
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &[u8] = b"peer-a";
    const B: &[u8] = b"peer-b";

    fn t() -> Transcript<'static> {
        Transcript { peer_id_a: A, peer_id_b: B, x25519_a: [1; 32], x25519_b: [2; 32] }
    }

    fn info(name: &str) -> PairInfo {
        PairInfo { name: name.into(), app_version: "0.6.0".into(), may_start_model: true }
    }

    fn keys(code_a: &str, code_b: &str, ids_a: (&[u8], &[u8]), ids_b: (&[u8], &[u8])) -> (SessionKey, SessionKey) {
        let (sa, ma) = PakeStart::new(PakeRole::Client, &PairingCode::parse(code_a).unwrap(), ids_a.0, ids_a.1);
        let (sb, mb) = PakeStart::new(PakeRole::Server, &PairingCode::parse(code_b).unwrap(), ids_b.0, ids_b.1);
        (sa.finish(&mb).unwrap(), sb.finish(&ma).unwrap())
    }

    #[test]
    fn code_generation_and_parsing() {
        for _ in 0..1000 {
            let c = PairingCode::generate();
            assert_eq!(c.as_str().len(), 6);
            assert!(c.as_str().bytes().all(|b| b.is_ascii_digit()));
        }
        assert_eq!(PairingCode::parse("123 456").unwrap().as_str(), "123456");
        assert_eq!(PairingCode::parse("012-345").unwrap().as_str(), "012345");
        for bad in ["12345", "1234567", "12a456", "", "١٢٣٤٥٦"] {
            assert_eq!(PairingCode::parse(bad).err(), Some(PakeError::BadCode), "{bad}");
        }
        assert!(!format!("{:?}", PairingCode::parse("123456").unwrap()).contains("123"));
    }

    #[test]
    fn correct_code_confirms_both_ways_and_exchanges_info() {
        let (ka, kb) = keys("424242", "424242", (A, B), (A, B));
        let ib = info("Mac Studio");
        let tag_b = kb.confirm(&t(), &ib, &[]);
        let sealed_b = kb.seal_info(&ib);
        assert_eq!(ka.open_info(&sealed_b).unwrap(), ib);
        ka.verify_peer(&t(), &ib, &[], &tag_b).unwrap();
        let ia = PairInfo { may_start_model: false, ..info("MacBook") };
        let tag_a = ka.confirm(&t(), &ia, &tag_b);
        assert_eq!(kb.open_info(&ka.seal_info(&ia)).unwrap(), ia);
        kb.verify_peer(&t(), &ia, &tag_b, &tag_a).unwrap();
        ka.open_ack(&kb.seal_ack()).unwrap();
    }

    #[test]
    fn wrong_code_fails_confirmation_and_info() {
        let (ka, kb) = keys("111111", "111112", (A, B), (A, B));
        let ib = info("Mac Studio");
        assert_eq!(ka.verify_peer(&t(), &ib, &[], &kb.confirm(&t(), &ib, &[])), Err(PakeError::Confirmation));
        assert_eq!(ka.open_info(&kb.seal_info(&ib)), Err(PakeError::Confirmation));
        assert!(ka.open_ack(&kb.seal_ack()).is_err());
    }

    #[test]
    fn identity_mismatch_relay_fails() {
        // A thinks it talks to M, B thinks M is the client: same code, different ids.
        let (ka, kb) = keys("424242", "424242", (A, b"mitm"), (b"mitm", B));
        let ib = info("Mac Studio");
        assert!(ka.verify_peer(&t(), &ib, &[], &kb.confirm(&t(), &ib, &[])).is_err());
    }

    #[test]
    fn replayed_or_reflected_confirmation_rejected() {
        let ib = info("Mac Studio");
        let (_ka1, kb1) = keys("424242", "424242", (A, B), (A, B));
        let old_tag = kb1.confirm(&t(), &ib, &[]);
        // same code, same ids, fresh session: the old tag does not verify
        let (ka2, kb2) = keys("424242", "424242", (A, B), (A, B));
        assert!(ka2.verify_peer(&t(), &ib, &[], &old_tag).is_err());
        // reflection: B's own tag sent back as A's tag
        let tag_b = kb2.confirm(&t(), &ib, &[]);
        assert!(kb2.verify_peer(&t(), &ib, &tag_b, &tag_b).is_err());
        assert!(kb2.verify_peer(&t(), &ib, &[], &tag_b).is_err());
    }

    #[test]
    fn every_transcript_field_is_bound() {
        let (ka, kb) = keys("424242", "424242", (A, B), (A, B));
        let ib = info("Mac Studio");
        let tag = kb.confirm(&t(), &ib, &[]);
        ka.verify_peer(&t(), &ib, &[], &tag).unwrap();
        let mut variants = vec![];
        let mut t1 = t();
        t1.x25519_a[0] ^= 1;
        variants.push((t1, ib.clone()));
        let mut t2 = t();
        t2.x25519_b[31] ^= 1;
        variants.push((t2, ib.clone()));
        variants.push((Transcript { peer_id_a: b"peer-x", ..t() }, ib.clone()));
        variants.push((Transcript { peer_id_b: b"peer-x", ..t() }, ib.clone()));
        variants.push((t(), info("Mac Studi0")));
        variants.push((t(), PairInfo { app_version: "0.6.1".into(), ..ib.clone() }));
        variants.push((t(), PairInfo { may_start_model: false, ..ib.clone() }));
        for (tt, ii) in variants {
            assert!(ka.verify_peer(&tt, &ii, &[], &tag).is_err());
        }
        assert!(ka.verify_peer(&t(), &ib, b"x", &tag).is_err());
        // length prefixes: moving bytes between fields changes the tag
        let i1 = PairInfo { name: "ab".into(), app_version: "c".into(), may_start_model: true };
        let i2 = PairInfo { name: "a".into(), app_version: "bc".into(), may_start_model: true };
        assert_ne!(kb.confirm(&t(), &i1, &[]), kb.confirm(&t(), &i2, &[]));
    }

    #[test]
    fn malformed_messages_rejected() {
        let (s, _) = PakeStart::new(PakeRole::Client, &PairingCode::parse("000000").unwrap(), A, B);
        assert_eq!(s.finish(&[0u8; 5]).err(), Some(PakeError::BadMessage));
        // a message from the same side (A→A) is refused
        let (s, _) = PakeStart::new(PakeRole::Client, &PairingCode::parse("000000").unwrap(), A, B);
        let (_, ma) = PakeStart::new(PakeRole::Client, &PairingCode::parse("000000").unwrap(), A, B);
        assert_eq!(s.finish(&ma).err(), Some(PakeError::BadMessage));
        let (ka, kb) = keys("424242", "424242", (A, B), (A, B));
        let mut sealed = kb.seal_info(&info("x"));
        sealed[0] ^= 1;
        assert!(ka.open_info(&sealed).is_err());
        let long = PairInfo { name: "x".repeat(MAX_CLAIM_LEN + 1), ..info("x") };
        assert_eq!(ka.open_info(&kb.seal_info(&long)), Err(PakeError::BadMessage));
    }
}
