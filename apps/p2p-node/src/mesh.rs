//! Product integration for sealed Slipstream inference over direct libp2p QUIC.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::request_response::{Event, Message, OutboundFailure};
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId};
use p2p_core::{JobRequest, JobResult, NodeId};
use p2p_crypto::{open_job_result, seal_job_request, NodeKeypair, SigningIdentity};
use p2p_libp2p::{build_quic_swarm_with_identity, MeshEvent};
use p2p_net::NetMessage;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

use crate::{hello_nonce, signed_hello, verify_signed_hello, RunningNode, RuntimeError};

const CHALLENGE_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_OUTSTANDING_CHALLENGES: usize = 4096;
const JOB_TIMEOUT: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MeshRequest {
    Probe {
        hello: NetMessage,
    },
    Infer {
        hello: NetMessage,
        encrypted_job: NetMessage,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MeshResponse {
    Hello { hello: NetMessage },
    Result { encrypted_result: NetMessage },
    Error { code: String },
}

#[derive(Debug)]
pub struct MeshJobOutcome {
    pub result: JobResult,
    pub worker_identity: String,
    pub worker_encryption_id: String,
    pub transport_peer_id: PeerId,
}

#[derive(Debug, Error)]
pub enum MeshRuntimeError {
    #[error("mesh protocol: {0}")]
    Protocol(String),
    #[error("mesh transport: {0}")]
    Transport(String),
    #[error("mesh request timed out")]
    Timeout,
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error("crypto: {0}")]
    Crypto(#[from] p2p_crypto::CryptoError),
    #[error("wire: {0}")]
    Wire(#[from] crate::WireError),
    #[error("identity: {0}")]
    Identity(String),
}

fn transport_identity(
    keypair: &NodeKeypair,
) -> Result<libp2p::identity::Keypair, MeshRuntimeError> {
    let signing = SigningIdentity::from_node_keypair(keypair);
    let expected_public = signing.public_bytes();
    let mut seed = signing.secret_seed();
    let identity = libp2p::identity::Keypair::ed25519_from_bytes(&mut seed)
        .map_err(|error| MeshRuntimeError::Identity(error.to_string()));
    seed.fill(0);
    let identity = identity?;
    let transport_public = identity
        .public()
        .try_into_ed25519()
        .map_err(|_| MeshRuntimeError::Identity("transport identity is not Ed25519".into()))?;
    if transport_public.to_bytes() != expected_public {
        return Err(MeshRuntimeError::Identity(
            "transport and signed Hello identity differ".into(),
        ));
    }
    Ok(identity)
}

pub fn transport_peer_id(keypair: &NodeKeypair) -> Result<PeerId, MeshRuntimeError> {
    Ok(transport_identity(keypair)?.public().to_peer_id())
}

pub struct MeshWorker {
    node: RunningNode,
    challenges: Mutex<HashMap<(PeerId, Vec<u8>), Instant>>,
}

impl MeshWorker {
    pub fn new(node: RunningNode) -> Self {
        Self {
            node,
            challenges: Mutex::new(HashMap::new()),
        }
    }

    pub fn handle_request(&self, peer: PeerId, bytes: &[u8]) -> Vec<u8> {
        let response = self
            .handle_request_inner(peer, bytes)
            .unwrap_or_else(|error| {
                warn!(%peer, error = %error, "mesh request rejected");
                MeshResponse::Error {
                    code: "invalid_request".into(),
                }
            });
        serde_json::to_vec(&response).expect("MeshResponse serialization is infallible")
    }

    fn handle_request_inner(
        &self,
        peer: PeerId,
        bytes: &[u8],
    ) -> Result<MeshResponse, MeshRuntimeError> {
        let request: MeshRequest = serde_json::from_slice(bytes)
            .map_err(|_| MeshRuntimeError::Protocol("invalid request encoding".into()))?;
        match request {
            MeshRequest::Probe { hello } => {
                verify_signed_hello(&hello, Some(&[]), None)?;
                bind_hello_to_transport(&hello, peer)?;
                let requester_challenge = hello_nonce(&hello)?;
                let response = signed_hello(
                    self.node.advert(),
                    self.node.config.keypair.as_ref(),
                    requester_challenge,
                )?;
                let worker_challenge = hello_nonce(&response)?;
                let mut challenges = self.challenges.lock().expect("mesh challenges");
                let now = Instant::now();
                challenges.retain(|_, issued| now.duration_since(*issued) <= CHALLENGE_TTL);
                if challenges.len() >= MAX_OUTSTANDING_CHALLENGES {
                    return Err(MeshRuntimeError::Protocol(
                        "challenge capacity reached".into(),
                    ));
                }
                challenges.insert((peer, worker_challenge), now);
                Ok(MeshResponse::Hello { hello: response })
            }
            MeshRequest::Infer {
                hello,
                encrypted_job,
            } => {
                let response_to = match &hello {
                    NetMessage::Hello {
                        auth: Some(auth), ..
                    } => auth.response_to.clone(),
                    _ => {
                        return Err(MeshRuntimeError::Protocol(
                            "authenticated Hello required".into(),
                        ))
                    }
                };
                let issued = self
                    .challenges
                    .lock()
                    .expect("mesh challenges")
                    .remove(&(peer, response_to.clone()));
                if issued.is_none_or(|instant| instant.elapsed() > CHALLENGE_TTL) {
                    return Err(MeshRuntimeError::Protocol(
                        "unknown or expired worker challenge".into(),
                    ));
                }
                let remote = verify_signed_hello(&hello, Some(&response_to), None)?;
                bind_hello_to_transport(&hello, peer)?;
                let encrypted_result = self.node.process_mesh_job(&remote, &encrypted_job)?;
                Ok(MeshResponse::Result { encrypted_result })
            }
        }
    }
}

pub async fn serve_mesh(node: RunningNode, listen: Multiaddr) -> Result<(), MeshRuntimeError> {
    validate_mesh_listen(node.config.policy.mode, &listen)?;
    let identity = transport_identity(node.config.keypair.as_ref())?;
    let mut swarm = build_quic_swarm_with_identity(identity).unwrap_or_else(|never| match never {});
    swarm
        .listen_on(listen)
        .map_err(|error| MeshRuntimeError::Transport(error.to_string()))?;
    let worker = MeshWorker::new(node);
    let peer_id = *swarm.local_peer_id();

    loop {
        match swarm.select_next_some().await {
            SwarmEvent::NewListenAddr { address, .. } => {
                println!(
                    "mesh listening {address}/p2p/{peer_id} identity={} encryption={} mode={} donate={} plaintext_boundary=selected_worker",
                    SigningIdentity::from_node_keypair(worker.node.config.keypair.as_ref())
                        .public_hex(),
                    worker.node.config.keypair.node_id(),
                    worker.node.config.policy.mode,
                    worker.node.config.policy.donate_capacity,
                );
            }
            SwarmEvent::Behaviour(MeshEvent::Inference(Event::Message {
                peer,
                message:
                    Message::Request {
                        request, channel, ..
                    },
                ..
            })) => {
                let response = worker.handle_request(peer, &request);
                if swarm
                    .behaviour_mut()
                    .send_response(channel, response)
                    .is_err()
                {
                    warn!(%peer, "mesh response channel closed");
                }
            }
            _ => {}
        }
    }
}

pub async fn send_mesh_job(
    peer_address: Multiaddr,
    keypair: &NodeKeypair,
    request: &JobRequest,
    expected_worker_identity: Option<&str>,
) -> Result<MeshJobOutcome, MeshRuntimeError> {
    let identity = transport_identity(keypair)?;
    let mut swarm = build_quic_swarm_with_identity(identity).unwrap_or_else(|never| match never {});
    swarm
        .dial(peer_address)
        .map_err(|error| MeshRuntimeError::Transport(error.to_string()))?;

    let ours = crate::capability_to_advert(
        &keypair.node_id(),
        &crate::default_capability(vec![request.model.clone()], true),
        true,
    );
    let probe_hello = signed_hello(ours.clone(), keypair, Vec::new())?;
    let requester_challenge = hello_nonce(&probe_hello)?;
    let probe = serde_json::to_vec(&MeshRequest::Probe { hello: probe_hello })
        .map_err(|error| MeshRuntimeError::Protocol(error.to_string()))?;

    let future = async {
        let mut transport_peer_id = None;
        let mut worker_identity = None;
        let mut worker_encryption_id = None;
        let mut probe_sent = false;
        loop {
            match swarm.select_next_some().await {
                SwarmEvent::ConnectionEstablished { peer_id, .. } if !probe_sent => {
                    transport_peer_id = Some(peer_id);
                    swarm.behaviour_mut().send_request(&peer_id, probe.clone());
                    probe_sent = true;
                }
                SwarmEvent::Behaviour(MeshEvent::Inference(Event::Message {
                    peer,
                    message: Message::Response { response, .. },
                    ..
                })) => {
                    let response: MeshResponse =
                        serde_json::from_slice(&response).map_err(|_| {
                            MeshRuntimeError::Protocol("invalid response encoding".into())
                        })?;
                    match response {
                        MeshResponse::Hello { hello } => {
                            let worker = verify_signed_hello(
                                &hello,
                                Some(&requester_challenge),
                                expected_worker_identity,
                            )?;
                            bind_hello_to_transport(&hello, peer)?;
                            let worker_challenge = hello_nonce(&hello)?;
                            let recipient = NodeId::from_hex(&worker.node_id)
                                .map_err(|error| MeshRuntimeError::Protocol(error.to_string()))?;
                            worker_identity = Some(worker.identity_id.clone());
                            worker_encryption_id = Some(worker.node_id.clone());
                            let sealed = seal_job_request(request, &recipient)?;
                            let encrypted_job = crate::sealed_to_net(&request.job_id, &sealed)?;
                            let client_hello =
                                signed_hello(ours.clone(), keypair, worker_challenge)?;
                            let infer = serde_json::to_vec(&MeshRequest::Infer {
                                hello: client_hello,
                                encrypted_job,
                            })
                            .map_err(|error| MeshRuntimeError::Protocol(error.to_string()))?;
                            swarm.behaviour_mut().send_request(&peer, infer);
                            transport_peer_id = Some(peer);
                        }
                        MeshResponse::Result { encrypted_result } => {
                            let NetMessage::EncryptedJobResult {
                                job_id,
                                ciphertext,
                                ephemeral_pubkey,
                                ..
                            } = encrypted_result
                            else {
                                return Err(MeshRuntimeError::Protocol(
                                    "cleartext result downgrade rejected".into(),
                                ));
                            };
                            let envelope = crate::net_to_sealed(&ciphertext, &ephemeral_pubkey)?;
                            let result = open_job_result(&envelope, keypair)?;
                            if result.job_id != job_id || result.job_id != request.job_id {
                                return Err(MeshRuntimeError::Protocol(
                                    "sealed result job_id mismatch".into(),
                                ));
                            }
                            return Ok(MeshJobOutcome {
                                result,
                                worker_identity: worker_identity.clone().ok_or_else(|| {
                                    MeshRuntimeError::Protocol("missing worker identity".into())
                                })?,
                                worker_encryption_id: worker_encryption_id.clone().ok_or_else(
                                    || {
                                        MeshRuntimeError::Protocol(
                                            "missing worker encryption id".into(),
                                        )
                                    },
                                )?,
                                transport_peer_id: transport_peer_id.unwrap_or(peer),
                            });
                        }
                        MeshResponse::Error { code } => {
                            return Err(MeshRuntimeError::Protocol(format!(
                                "worker rejected request: {code}"
                            )));
                        }
                    }
                }
                SwarmEvent::Behaviour(MeshEvent::Inference(Event::OutboundFailure {
                    error: OutboundFailure::Timeout,
                    ..
                })) => return Err(MeshRuntimeError::Timeout),
                SwarmEvent::Behaviour(MeshEvent::Inference(Event::OutboundFailure {
                    error,
                    ..
                })) => return Err(MeshRuntimeError::Transport(error.to_string())),
                _ => {}
            }
        }
    };
    tokio::time::timeout(JOB_TIMEOUT, future)
        .await
        .map_err(|_| MeshRuntimeError::Timeout)?
}

fn bind_hello_to_transport(
    hello: &NetMessage,
    transport_peer: PeerId,
) -> Result<(), MeshRuntimeError> {
    let NetMessage::Hello {
        auth: Some(auth), ..
    } = hello
    else {
        return Err(MeshRuntimeError::Protocol(
            "authenticated Hello required".into(),
        ));
    };
    let ed25519 = libp2p::identity::ed25519::PublicKey::try_from_bytes(&auth.identity_pubkey)
        .map_err(|_| MeshRuntimeError::Identity("invalid Hello Ed25519 key".into()))?;
    let signed_peer = libp2p::identity::PublicKey::from(ed25519).to_peer_id();
    if signed_peer != transport_peer {
        return Err(MeshRuntimeError::Protocol(
            "signed Hello identity does not match authenticated QUIC peer".into(),
        ));
    }
    Ok(())
}

pub fn validate_mesh_listen(
    mode: crate::NodeMode,
    address: &Multiaddr,
) -> Result<(), MeshRuntimeError> {
    if mode != crate::NodeMode::Local {
        return Ok(());
    }
    let loopback = address.iter().any(|protocol| match protocol {
        Protocol::Ip4(ip) => ip.is_loopback(),
        Protocol::Ip6(ip) => ip.is_loopback(),
        _ => false,
    });
    if loopback {
        Ok(())
    } else {
        Err(MeshRuntimeError::Protocol(
            "local mode requires an explicit loopback mesh listen address".into(),
        ))
    }
}
