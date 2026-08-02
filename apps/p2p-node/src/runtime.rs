//! Node serve loop: Hello exchange, sealed jobs, mock/real inference, credit settle.
//!
//! Real engine process spawn is opt-in (`EngineChoice` + `--spawn-engine` +
//! `p2p-engine`/`p2p-node` `launch` feature). Default remains mock.
//!
//! `--spawn-engine` refuses when the oMLX/PGRN serve lock is live or the
//! configured infer endpoint already answers (dual-serve freeze risk).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Child;
use std::sync::{Arc, Mutex};

use p2p_core::{
    local_capability, BackendKind, Capability, InferenceEngine, JobRequest, JobResult, NodeId,
};
use p2p_crypto::{open_job_request, open_job_result, seal_job_result, NodeKeypair};
use p2p_engine::{
    launch_serve_plan, open_engine_for_choice_at, plan_serve_for_choice, stop_child, EngineChoice,
};
use p2p_ledger::Ledger;
use p2p_net::message::CapabilityAdvert;
use p2p_net::{session, NetMessage, PeerSession};
use p2p_security::ReplayCache;
use thiserror::Error;
use tracing::{info, warn};

use crate::spawn_guard::{check_spawn_engine_safe, default_lock_path, resolve_guard_endpoint};
use crate::wire::{net_to_sealed, sealed_result_to_net, WireError};
use crate::{AdmissionController, NodeMode, NodePolicy};

/// MVP faucet: auto-fund consumers so settle stubs always succeed in local demos.
const FAUCET_CREDITS: u64 = 1_000;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("net: {0}")]
    Net(#[from] p2p_net::NetError),
    #[error("crypto: {0}")]
    Crypto(#[from] p2p_crypto::CryptoError),
    #[error("ledger: {0}")]
    Ledger(#[from] p2p_ledger::LedgerError),
    #[error("wire: {0}")]
    Wire(#[from] WireError),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("engine: {0}")]
    Engine(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone)]
pub struct NodeConfig {
    pub listen: SocketAddr,
    pub keypair: Arc<NodeKeypair>,
    pub capability: Capability,
    /// Engine selection (default [`EngineChoice::Mock`]).
    pub engine: EngineChoice,
    /// When true (and `engine` is not mock), spawn the planned serve process.
    /// Requires the `launch` feature; otherwise [`RunningNode::open`] errors.
    pub spawn_engine: bool,
    pub ledger_path: Option<PathBuf>,
    /// Comma-separated bootstrap peers (dialed at start for discovery).
    pub bootstrap: Vec<SocketAddr>,
    /// Network exposure and resource policy. Safe default is local-only.
    pub policy: NodePolicy,
}

impl NodeConfig {
    pub fn force_mock(&self) -> bool {
        self.engine.is_mock()
    }
}

pub struct RunningNode {
    pub config: NodeConfig,
    pub listen_addr: SocketAddr,
    pub ledger: Ledger,
    pub peers: Arc<Mutex<HashMap<String, CapabilityAdvert>>>,
    replay: Arc<Mutex<ReplayCache>>,
    admission: AdmissionController,
    engine: Arc<dyn InferenceEngine>,
    /// Optional child from `--spawn-engine` (killed on drop).
    engine_child: Option<Child>,
}

impl Drop for RunningNode {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.engine_child {
            stop_child(child);
        }
    }
}

impl RunningNode {
    pub fn open(config: NodeConfig) -> Result<Self, RuntimeError> {
        config
            .policy
            .validate_for_listen(config.listen)
            .map_err(|e| RuntimeError::Protocol(format!("invalid node policy: {e}")))?;
        let replay = Arc::new(Mutex::new(ReplayCache::with_capacity(
            std::time::Duration::from_secs(24 * 60 * 60),
            config.policy.max_replay_entries,
        )));
        let admission = AdmissionController::new(config.policy.clone());
        let ledger = match &config.ledger_path {
            Some(path) => Ledger::open_sqlite(path)?,
            None => Ledger::open_memory()?,
        };
        // Provider starts with zero earned credits; faucet funds consumers on Hello.
        let _ = ledger.balance(config.keypair.node_id().as_hex())?;

        let mut engine_child = None;
        let mut infer_endpoint: Option<String> = None;
        if config.spawn_engine {
            if config.engine.is_mock() {
                return Err(RuntimeError::Engine(
                    "--spawn-engine requires --engine mlx|llama|auto (not mock)".into(),
                ));
            }
            if !p2p_engine::launch_feature_enabled() {
                return Err(RuntimeError::Engine(
                    "engine spawn disabled: rebuild with `cargo run -p p2p-node --features launch` \
                     (or use --dry-run-engine to print the argv)"
                        .into(),
                ));
            }
            // Freeze contract: never dual-serve against a live product/oMLX lock or
            // an already-healthy Slipstream/llama endpoint.
            let guard_endpoint = resolve_guard_endpoint(config.engine, &config.capability.os);
            check_spawn_engine_safe(&default_lock_path(), &guard_endpoint)
                .map_err(RuntimeError::Engine)?;
            let plan = plan_serve_for_choice(config.engine, &config.capability.os)
                .map_err(RuntimeError::Engine)?;
            infer_endpoint = plan.http_endpoint();
            if let Some(ref ep) = infer_endpoint {
                if crate::spawn_guard::normalize_endpoint(ep) != guard_endpoint {
                    check_spawn_engine_safe(&default_lock_path(), ep)
                        .map_err(RuntimeError::Engine)?;
                }
            }
            info!(
                plan = %plan.display(),
                endpoint = infer_endpoint.as_deref().unwrap_or("(unknown)"),
                "spawning engine serve"
            );
            let child = launch_serve_plan(&plan).map_err(RuntimeError::Engine)?;
            engine_child = Some(child);
        }

        let engine: Arc<dyn InferenceEngine> = Arc::from(open_engine_for_choice_at(
            config.engine,
            &config.capability.os,
            &config.capability,
            infer_endpoint.as_deref(),
        ));
        if !config.engine.is_mock() {
            if let Some(ref ep) = infer_endpoint {
                info!(endpoint = %ep, "HTTP infer target");
            }
        }
        Ok(Self {
            config,
            listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            ledger,
            peers: Arc::new(Mutex::new(HashMap::new())),
            replay,
            admission,
            engine,
            engine_child,
        })
    }

    pub fn advert(&self) -> CapabilityAdvert {
        capability_to_advert(
            &self.config.keypair.node_id(),
            &self.config.capability,
            self.config.force_mock(),
        )
    }

    pub fn list_peers(&self) -> Vec<CapabilityAdvert> {
        self.peers
            .lock()
            .expect("peers")
            .values()
            .cloned()
            .collect()
    }

    /// Bind, print `listening <addr>`, then accept forever.
    pub async fn serve(mut self) -> Result<(), RuntimeError> {
        let (listener, addr) = self.bind().await?;
        println!(
            "listening {} node_id={} engine={} spawn={}",
            addr,
            self.config.keypair.node_id(),
            self.config.engine.as_str(),
            self.engine_child.is_some()
        );
        self.accept_loop(listener).await
    }

    /// Bind only (for tests). Sets [`Self::listen_addr`].
    pub async fn bind(&mut self) -> Result<(tokio::net::TcpListener, SocketAddr), RuntimeError> {
        let (listener, addr) = p2p_net::listen(self.config.listen).await?;
        self.listen_addr = addr;
        info!(
            node_id = %self.config.keypair.node_id(),
            %addr,
            "p2p-node bound"
        );

        let bootstrap = self.config.bootstrap.clone();
        for peer in bootstrap {
            match dial_hello(self, peer).await {
                Ok(remote) => {
                    info!(peer = %peer, remote_id = %remote.node_id, "bootstrap hello ok");
                    self.peers
                        .lock()
                        .expect("peers")
                        .insert(remote.node_id.clone(), remote);
                }
                Err(e) => warn!(peer = %peer, error = %e, "bootstrap hello failed"),
            }
        }
        Ok((listener, addr))
    }

    pub async fn accept_loop(self, listener: tokio::net::TcpListener) -> Result<(), RuntimeError> {
        loop {
            let session = session::accept(&listener).await?;
            let node_id = self.config.keypair.node_id().as_hex().to_string();
            let advert = self.advert();
            let keypair = Arc::clone(&self.config.keypair);
            let engine = Arc::clone(&self.engine);
            let ledger = self.ledger.clone();
            let peers = Arc::clone(&self.peers);
            let replay = Arc::clone(&self.replay);
            let admission = self.admission.clone();
            let community_free = self.config.policy.mode == NodeMode::Community;
            tokio::spawn(async move {
                if let Err(e) = handle_session(
                    session,
                    advert,
                    keypair,
                    engine,
                    ledger,
                    peers,
                    replay,
                    admission,
                    community_free,
                    node_id,
                )
                .await
                {
                    warn!(error = %e, "session ended with error");
                }
            });
        }
    }
}

async fn dial_hello(
    node: &RunningNode,
    addr: SocketAddr,
) -> Result<CapabilityAdvert, RuntimeError> {
    let mut session = p2p_net::connect(addr).await?;
    let remote = session.exchange_hello(node.advert()).await?;
    Ok(remote)
}

async fn handle_session(
    mut session: PeerSession,
    advert: CapabilityAdvert,
    keypair: Arc<NodeKeypair>,
    engine: Arc<dyn InferenceEngine>,
    ledger: Ledger,
    peers: Arc<Mutex<HashMap<String, CapabilityAdvert>>>,
    replay: Arc<Mutex<ReplayCache>>,
    admission: AdmissionController,
    community_free: bool,
    provider_id: String,
) -> Result<(), RuntimeError> {
    // Accepting side: recv Hello first, then reply (avoids relying on concurrent send).
    let remote = match session.recv().await? {
        NetMessage::Hello { capability } => capability,
        other => {
            return Err(RuntimeError::Protocol(format!(
                "expected Hello, got {other:?}"
            )))
        }
    };
    session
        .send(&NetMessage::Hello { capability: advert })
        .await?;
    peers
        .lock()
        .expect("peers")
        .insert(remote.node_id.clone(), remote.clone());

    // Private/local compatibility uses the demo ledger. Permissionless community
    // jobs are donation-based and never depend on a global credit faucet.
    if !community_free {
        let _ = ledger.fund(&remote.node_id, FAUCET_CREDITS);
    }
    let consumer = NodeId::from_hex(&remote.node_id).map_err(|e| {
        RuntimeError::Protocol(format!("peer Hello node_id not a valid NodeId: {e}"))
    })?;
    loop {
        let msg = match session.recv().await {
            Ok(m) => {
                let admitted = {
                    let mut cache = replay.lock().expect("replay cache");
                    p2p_net::admit_encrypted_job(&m, &mut cache)
                };
                match admitted {
                    Ok(()) => m,
                    Err(e) if e.is_replay() => {
                        warn!(error = %e, "replay rejected");
                        let nack = JobResult::failure("replay", "replay");
                        match seal_result_message(&nack, &consumer) {
                            Ok(msg) => {
                                let _ = session.send(&msg).await;
                            }
                            Err(seal_err) => {
                                warn!(error = %seal_err, "failed to seal replay nack");
                            }
                        }
                        continue;
                    }
                    Err(e) => return Err(RuntimeError::Net(e)),
                }
            }
            Err(e) if e.is_replay() => {
                warn!(error = %e, "replay rejected");
                let nack = JobResult::failure("replay", "replay");
                match seal_result_message(&nack, &consumer) {
                    Ok(msg) => {
                        let _ = session.send(&msg).await;
                    }
                    Err(seal_err) => {
                        warn!(error = %seal_err, "failed to seal replay nack");
                    }
                }
                continue;
            }
            Err(_) => break,
        };
        match msg {
            NetMessage::Heartbeat { seq } => {
                session.send(&NetMessage::Heartbeat { seq }).await?;
            }
            NetMessage::EncryptedJob {
                job_id,
                ciphertext,
                nonce,
                ephemeral_pubkey,
            } => {
                let frame_bytes =
                    sealed_job_frame_bytes(&job_id, &ciphertext, &nonce, &ephemeral_pubkey);
                let result = match admission.validate_frame(frame_bytes) {
                    Ok(()) => process_job(
                        &job_id,
                        &ciphertext,
                        &ephemeral_pubkey,
                        &keypair,
                        engine.as_ref(),
                        &ledger,
                        &remote.node_id,
                        &provider_id,
                        &admission,
                        frame_bytes,
                        community_free,
                    ),
                    Err(rejection) => JobResult::failure(&job_id, rejection.as_code()),
                };
                // TM-007: seal JobResult to the consumer (never cleartext on product path).
                let msg = seal_result_message(&result, &consumer)?;
                session.send(&msg).await?;
            }
            NetMessage::Hello { capability } => {
                peers
                    .lock()
                    .expect("peers")
                    .insert(capability.node_id.clone(), capability);
            }
            NetMessage::EncryptedJobResult { .. } | NetMessage::JobResult { .. } => {
                // Ignore unexpected results on the worker.
            }
        }
    }
    Ok(())
}

/// Seal a [`JobResult`] into [`NetMessage::EncryptedJobResult`] for `consumer`.
fn seal_result_message(result: &JobResult, consumer: &NodeId) -> Result<NetMessage, RuntimeError> {
    let sealed = seal_job_result(result, consumer)?;
    Ok(sealed_result_to_net(&result.job_id, &sealed)?)
}

fn process_job(
    job_id: &str,
    ciphertext: &[u8],
    ephemeral_pubkey: &[u8],
    keypair: &NodeKeypair,
    engine: &dyn InferenceEngine,
    ledger: &Ledger,
    consumer_id: &str,
    provider_id: &str,
    admission: &AdmissionController,
    frame_bytes: u32,
    community_free: bool,
) -> JobResult {
    let sealed = match net_to_sealed(ciphertext, ephemeral_pubkey) {
        Ok(s) => s,
        Err(e) => return JobResult::failure(job_id, e.to_string()),
    };
    let request = match open_job_request(&sealed, keypair) {
        Ok(r) => r,
        Err(e) => return JobResult::failure(job_id, e.to_string()),
    };
    // Prefer wire job_id if present; fall back to plaintext.
    let mut request = request;
    if request.job_id.is_empty() {
        request.job_id = job_id.to_string();
    }
    let _permit = match admission.admit(consumer_id, request.max_tokens, frame_bytes) {
        Ok(permit) => permit,
        Err(rejection) => {
            return JobResult::failure(job_id, rejection.as_code());
        }
    };
    let result = engine.infer(&request);
    if result.ok && !community_free {
        if let Err(e) = ledger.settle(
            &result.job_id,
            consumer_id,
            provider_id,
            u64::from(result.tokens),
        ) {
            warn!(error = %e, job_id = %result.job_id, "settle failed");
        }
    }
    result
}

fn sealed_job_frame_bytes(
    job_id: &str,
    ciphertext: &[u8],
    nonce: &[u8],
    ephemeral_pubkey: &[u8],
) -> u32 {
    let bytes = job_id
        .len()
        .saturating_add(ciphertext.len())
        .saturating_add(nonce.len())
        .saturating_add(ephemeral_pubkey.len());
    u32::try_from(bytes).unwrap_or(u32::MAX)
}

pub fn capability_to_advert(id: &NodeId, cap: &Capability, force_mock: bool) -> CapabilityAdvert {
    let backend = if force_mock {
        "mock".into()
    } else {
        match cap.backend {
            BackendKind::Mlx => "mlx".into(),
            BackendKind::LlamaPgrn => "llama_pgrn".into(),
        }
    };
    CapabilityAdvert {
        node_id: id.as_hex().to_string(),
        models: cap.models.clone(),
        ram_gib: cap.ram_gib,
        vram_gib: cap.vram_gib,
        backend,
    }
}

pub fn default_capability(models: Vec<String>, _force_mock: bool) -> Capability {
    // OS drives Capability.backend; advert string uses "mock" when force_mock.
    local_capability(std::env::consts::OS, 36, 0, models)
}

/// Adjust capability.backend when an explicit engine choice overrides the OS matrix.
pub fn capability_for_engine(mut cap: Capability, choice: EngineChoice) -> Capability {
    if let Some(kind) = choice.to_backend(&cap.os) {
        cap.backend = kind;
    }
    cap
}

pub async fn client_hello(
    addr: SocketAddr,
    ours: CapabilityAdvert,
) -> Result<(PeerSession, CapabilityAdvert), RuntimeError> {
    let mut session = p2p_net::connect(addr).await?;
    // Dialer sends Hello first (matches exchange_hello / worker recv-first).
    session
        .send(&NetMessage::Hello { capability: ours })
        .await?;
    let remote = match session.recv().await? {
        NetMessage::Hello { capability } => capability,
        other => {
            return Err(RuntimeError::Protocol(format!(
                "expected Hello, got {other:?}"
            )))
        }
    };
    Ok((session, remote))
}

/// Seal + send a job; open the sealed [`NetMessage::EncryptedJobResult`] reply.
///
/// `opener` is the consumer identity (must match the Hello `node_id` the worker
/// sealed to). Cleartext [`NetMessage::JobResult`] is accepted only as a
/// loopback/test exception (TM-007); product workers always seal.
pub async fn send_sealed_job(
    session: &mut PeerSession,
    request: &JobRequest,
    recipient: &NodeId,
    opener: &NodeKeypair,
) -> Result<JobResult, RuntimeError> {
    use p2p_crypto::seal_job_request;

    let sealed = seal_job_request(request, recipient)?;
    let msg = crate::wire::sealed_to_net(&request.job_id, &sealed)?;
    session.send(&msg).await?;
    match session.recv().await? {
        NetMessage::EncryptedJobResult {
            job_id,
            ciphertext,
            ephemeral_pubkey,
            ..
        } => {
            let envelope = net_to_sealed(&ciphertext, &ephemeral_pubkey)?;
            let result = open_job_result(&envelope, opener)?;
            // TM-010: bind wire job_id to opened payload.
            if result.job_id != job_id {
                return Err(RuntimeError::Protocol(format!(
                    "sealed result job_id mismatch: wire={job_id} payload={}",
                    result.job_id
                )));
            }
            Ok(result)
        }
        // Loopback / unit-test exception — do not use on LAN product path.
        NetMessage::JobResult {
            job_id,
            ok,
            text,
            tokens,
            error,
        } => Ok(JobResult {
            job_id,
            ok,
            text,
            tokens,
            error,
        }),
        other => Err(RuntimeError::Protocol(format!(
            "expected EncryptedJobResult, got {}",
            other.wire_type()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advert_backend_mock_label() {
        let kp = NodeKeypair::generate();
        let cap = default_capability(vec!["mock".into()], true);
        let a = capability_to_advert(&kp.node_id(), &cap, true);
        assert_eq!(a.backend, "mock");
        assert_eq!(a.node_id.len(), 64);
    }

    #[test]
    fn capability_for_engine_overrides_os() {
        let cap = default_capability(vec!["m".into()], false);
        let mlx = capability_for_engine(cap.clone(), EngineChoice::Mlx);
        assert_eq!(mlx.backend, BackendKind::Mlx);
        let llama = capability_for_engine(cap, EngineChoice::Llama);
        assert_eq!(llama.backend, BackendKind::LlamaPgrn);
    }

    #[test]
    fn open_rejects_spawn_without_launch_feature() {
        let kp = Arc::new(NodeKeypair::generate());
        let cfg = NodeConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            keypair: kp,
            capability: default_capability(vec!["m".into()], false),
            engine: EngineChoice::Llama,
            spawn_engine: true,
            ledger_path: None,
            bootstrap: vec![],
            policy: NodePolicy::default(),
        };
        match RunningNode::open(cfg) {
            Ok(_) => panic!("expected spawn open to fail without binaries / launch"),
            Err(e) => {
                let err = e.to_string();
                assert!(
                    err.contains("launch")
                        || err.contains("spawn")
                        || err.contains("not found")
                        || err.contains("missing")
                        || err.contains("engine"),
                    "{err}"
                );
            }
        }
    }
}
