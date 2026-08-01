//! Ephemeral X25519 + ChaCha20-Poly1305 sealed envelopes.
//!
//! Transit protection only: the recipient node decrypts to plaintext before
//! inference. This is **not** FHE / blind compute.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use hkdf::Hkdf;
use p2p_core::{JobEnvelopeMeta, JobRequest, JobResult, NodeId};
use rand::rngs::OsRng;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::error::CryptoError;
use crate::identity::NodeKeypair;

/// Domain separation for HKDF (`info`). Bump when the KDF layout changes.
const HKDF_INFO: &[u8] = b"slipstream-p2p-crypto/v1";

/// Wire-safe sealed blob (JSON-friendly hex fields).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedEnvelope {
    /// Ephemeral sender X25519 public key (32 bytes, lowercase hex).
    pub eph_pub_hex: String,
    /// ChaCha20-Poly1305 ciphertext including the Poly1305 tag (hex).
    pub ciphertext_hex: String,
}

/// Routed sealed job: plaintext routing meta + AEAD-sealed payload.
///
/// `meta` is intentionally not secret (routers need `to` / `model`). The
/// prompt/system live inside [`SealedEnvelope`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedJob {
    pub meta: JobEnvelopeMeta,
    pub payload: SealedEnvelope,
}

/// Seal raw bytes to a recipient X25519 public key.
///
/// Uses a fresh ephemeral X25519 keypair per call; shared secret is
/// HKDF-SHA256-expanded into a ChaCha20-Poly1305 key + nonce.
pub fn seal(plaintext: &[u8], recipient_pub: &PublicKey) -> Result<SealedEnvelope, CryptoError> {
    let eph_secret = StaticSecret::random_from_rng(OsRng);
    let eph_pub = PublicKey::from(&eph_secret);
    let shared = eph_secret.diffie_hellman(recipient_pub);
    let (key, nonce) = derive_key_nonce(shared.as_bytes(), eph_pub.as_bytes())?;
    let cipher = ChaCha20Poly1305::new((&key).into());
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext)
        .map_err(|_| CryptoError::Encrypt)?;
    Ok(SealedEnvelope {
        eph_pub_hex: hex::encode(eph_pub.as_bytes()),
        ciphertext_hex: hex::encode(ct),
    })
}

/// Seal to a [`NodeId`] that is an X25519 public key (see [`NodeKeypair::node_id`]).
pub fn seal_to_node_id(plaintext: &[u8], recipient: &NodeId) -> Result<SealedEnvelope, CryptoError> {
    let pub_key = public_key_from_bytes(&recipient.to_bytes())?;
    seal(plaintext, &pub_key)
}

/// Open an envelope with the recipient's static keypair.
pub fn open(envelope: &SealedEnvelope, identity: &NodeKeypair) -> Result<Vec<u8>, CryptoError> {
    let eph_bytes = decode_key(&envelope.eph_pub_hex)?;
    let eph_pub = PublicKey::from(eph_bytes);
    let shared = identity.secret().diffie_hellman(&eph_pub);
    let (key, nonce) = derive_key_nonce(shared.as_bytes(), eph_pub.as_bytes())?;
    let cipher = ChaCha20Poly1305::new((&key).into());
    let ct = hex::decode(&envelope.ciphertext_hex).map_err(|_| CryptoError::Decrypt)?;
    cipher
        .decrypt(Nonce::from_slice(&nonce), ct.as_ref())
        .map_err(|_| CryptoError::Decrypt)
}

/// Seal any JSON-serializable value to `recipient_pub`.
pub fn seal_json<T: Serialize>(
    value: &T,
    recipient_pub: &PublicKey,
) -> Result<SealedEnvelope, CryptoError> {
    let bytes = serde_json::to_vec(value).map_err(|e| CryptoError::Serde(e.to_string()))?;
    seal(&bytes, recipient_pub)
}

/// Open and JSON-deserialize into `T`.
pub fn open_json<T: DeserializeOwned>(
    envelope: &SealedEnvelope,
    identity: &NodeKeypair,
) -> Result<T, CryptoError> {
    let bytes = open(envelope, identity)?;
    serde_json::from_slice(&bytes).map_err(|e| CryptoError::Serde(e.to_string()))
}

/// Seal a [`JobRequest`] to the recipient's X25519 public [`NodeId`].
pub fn seal_job_request(
    request: &JobRequest,
    recipient: &NodeId,
) -> Result<SealedEnvelope, CryptoError> {
    let pub_key = public_key_from_bytes(&recipient.to_bytes())?;
    seal_json(request, &pub_key)
}

/// Open a sealed [`JobRequest`].
pub fn open_job_request(
    envelope: &SealedEnvelope,
    identity: &NodeKeypair,
) -> Result<JobRequest, CryptoError> {
    open_json(envelope, identity)
}

/// Seal a [`JobResult`] to the recipient's X25519 public [`NodeId`].
pub fn seal_job_result(
    result: &JobResult,
    recipient: &NodeId,
) -> Result<SealedEnvelope, CryptoError> {
    let pub_key = public_key_from_bytes(&recipient.to_bytes())?;
    seal_json(result, &pub_key)
}

/// Open a sealed [`JobResult`].
pub fn open_job_result(
    envelope: &SealedEnvelope,
    identity: &NodeKeypair,
) -> Result<JobResult, CryptoError> {
    open_json(envelope, identity)
}

/// Build [`JobEnvelopeMeta`] and seal the [`JobRequest`] payload to `to`.
///
/// `from` / `to` must be X25519-derived [`NodeId`]s ([`NodeKeypair::node_id`]).
/// Do **not** use [`p2p_core::NodeIdentity`] stub ids (XOR fingerprint ≠ X25519).
/// // TODO(core): once core NodeIdentity carries real X25519 pubs, accept it here.
pub fn seal_job(
    request: &JobRequest,
    from: &NodeId,
    to: &NodeId,
) -> Result<SealedJob, CryptoError> {
    let payload = seal_job_request(request, to)?;
    Ok(SealedJob {
        meta: JobEnvelopeMeta {
            job_id: request.job_id.clone(),
            from: from.clone(),
            to: to.clone(),
            model: request.model.clone(),
        },
        payload,
    })
}

/// Open the payload of a [`SealedJob`] as a [`JobRequest`].
pub fn open_job(sealed: &SealedJob, identity: &NodeKeypair) -> Result<JobRequest, CryptoError> {
    open_job_request(&sealed.payload, identity)
}

pub fn public_key_from_hex(hex_str: &str) -> Result<PublicKey, CryptoError> {
    Ok(PublicKey::from(decode_key(hex_str)?))
}

fn public_key_from_bytes(bytes: &[u8; 32]) -> Result<PublicKey, CryptoError> {
    Ok(PublicKey::from(*bytes))
}

fn derive_key_nonce(shared: &[u8], eph_pub: &[u8]) -> Result<([u8; 32], [u8; 12]), CryptoError> {
    // Salt = ephemeral pub so two seals with the same static DH still diverge
    // when ephemeral keys differ (they always should).
    let hk = Hkdf::<Sha256>::new(Some(eph_pub), shared);
    let mut okm = [0u8; 44];
    hk.expand(HKDF_INFO, &mut okm)
        .map_err(|_| CryptoError::Hkdf)?;
    let mut key = [0u8; 32];
    let mut nonce = [0u8; 12];
    key.copy_from_slice(&okm[..32]);
    nonce.copy_from_slice(&okm[32..44]);
    Ok((key, nonce))
}

fn decode_key(hex_str: &str) -> Result<[u8; 32], CryptoError> {
    let bytes = hex::decode(hex_str).map_err(|_| CryptoError::BadKey)?;
    if bytes.len() != 32 {
        return Err(CryptoError::BadKey);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> JobRequest {
        JobRequest {
            job_id: "job-1".into(),
            model: "mock".into(),
            system: "sys".into(),
            prompt: "hello sealed mesh".into(),
            max_tokens: 32,
        }
    }

    #[test]
    fn seal_open_bytes_roundtrip() {
        let bob = NodeKeypair::generate();
        let msg = b"prompt: hello mesh";
        let sealed = seal(msg, &bob.public_key()).unwrap();
        let opened = open(&sealed, &bob).unwrap();
        assert_eq!(opened, msg);
    }

    #[test]
    fn wrong_key_fails() {
        let alice = NodeKeypair::generate();
        let bob = NodeKeypair::generate();
        let sealed = seal(b"secret", &bob.public_key()).unwrap();
        assert_eq!(open(&sealed, &alice).unwrap_err(), CryptoError::Decrypt);
    }

    #[test]
    fn tamper_fails() {
        let bob = NodeKeypair::generate();
        let mut sealed = seal(b"secret", &bob.public_key()).unwrap();
        let mut bytes = hex::decode(&sealed.ciphertext_hex).unwrap();
        bytes[0] ^= 0xff;
        sealed.ciphertext_hex = hex::encode(bytes);
        assert_eq!(open(&sealed, &bob).unwrap_err(), CryptoError::Decrypt);
    }

    #[test]
    fn seal_to_node_id_roundtrip() {
        let bob = NodeKeypair::generate();
        let sealed = seal_to_node_id(b"via-node-id", &bob.node_id()).unwrap();
        assert_eq!(open(&sealed, &bob).unwrap(), b"via-node-id");
    }

    #[test]
    fn job_request_seal_open_roundtrip() {
        let alice = NodeKeypair::generate();
        let bob = NodeKeypair::generate();
        let req = sample_request();
        let sealed = seal_job_request(&req, &bob.node_id()).unwrap();
        let opened = open_job_request(&sealed, &bob).unwrap();
        assert_eq!(opened, req);
        assert!(open_job_request(&sealed, &alice).is_err());
    }

    #[test]
    fn job_result_seal_open_roundtrip() {
        let alice = NodeKeypair::generate();
        let result = JobResult::success("job-1", "tok a tok b", 2);
        let sealed = seal_job_result(&result, &alice.node_id()).unwrap();
        assert_eq!(open_job_result(&sealed, &alice).unwrap(), result);
    }

    #[test]
    fn sealed_job_includes_meta_and_payload() {
        let alice = NodeKeypair::generate();
        let bob = NodeKeypair::generate();
        let req = sample_request();
        let sealed = seal_job(&req, &alice.node_id(), &bob.node_id()).unwrap();
        assert_eq!(sealed.meta.job_id, "job-1");
        assert_eq!(sealed.meta.from, alice.node_id());
        assert_eq!(sealed.meta.to, bob.node_id());
        assert_eq!(sealed.meta.model, "mock");
        assert_eq!(open_job(&sealed, &bob).unwrap(), req);
        assert!(open_job(&sealed, &alice).is_err());
    }

    #[test]
    fn sealed_envelope_serde_roundtrip() {
        let bob = NodeKeypair::generate();
        let sealed = seal(b"serde", &bob.public_key()).unwrap();
        let json = serde_json::to_string(&sealed).unwrap();
        let back: SealedEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sealed);
        assert_eq!(open(&back, &bob).unwrap(), b"serde");
    }
}
