//! Node serve loop: Hello exchange, sealed jobs, mock/real inference, credit settle.
//!
//! Real engine process spawn is opt-in (`EngineChoice` + `--spawn-engine` +
//! `p2p-engine`/`p2p-node` `launch` feature). Default remains mock.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Child;
use std::sync::{Arc, Mutex};

use p2p_core::{local_capability, BackendKind, Capability, InferenceEngine, JobRequest, NodeId};
use p2p_crypto::{open_job_request, NodeKeypair};
use p2p_engine::{
    launch_serve_plan, open_engine_for_choice_at, plan_serve_for_choice, stop_child, EngineChoice,
};
use p2p_ledger::Ledger;
use p2p_net::message::CapabilityAdvert;
use p2p_net::{session, NetMessage, PeerSession};
use p2p_security::ReplayCache;
use thiserror::Error;
use tracing::{info, warn};

use crate::wire::{net_to_sealed, WireError};

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
            let plan = plan_serve_for_choice(config.engine, &config.capability.os)
                .map_err(RuntimeError::Engine)?;
            infer_endpoint = plan.http_endpoint();
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
            tokio::spawn(async move {
                if let Err(e) =
                    handle_session(session, advert, keypair, engine, ledger, peers, node_id).await
                {
                    warn!(error = %e, "session ended with error");
                }
            });
        }
    }
}

async fn dial_hello(node: &RunningNode, addr: SocketAddr) -> Result<CapabilityAdvert, RuntimeError> {
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
        .send(&NetMessage::Hello {
            capability: advert,
        })
        .await?;
    peers
        .lock()
        .expect("peers")
        .insert(remote.node_id.clone(), remote.clone());

    // MVP faucet so settle stubs work without a separate wallet CLI.
    let _ = ledger.fund(&remote.node_id, FAUCET_CREDITS);
    let mut replay = ReplayCache::with_default_ttl();

    loop {
        let msg = match session.recv_with_replay(&mut replay).await {
            Ok(m) => m,
            Err(e) if e.is_replay() => {
                warn!(error = %e, "replay rejected");
                let _ = session
                    .send(&NetMessage::JobResult {
                        job_id: "replay".into(),
                        ok: false,
                        text: String::new(),
                        tokens: 0,
                        error: Some("replay".into()),
                    })
                    .await;
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
                ephemeral_pubkey,
                ..
            } => {
                let result = process_job(
                    &job_id,
                    &ciphertext,
                    &ephemeral_pubkey,
                    &keypair,
                    engine.as_ref(),
                    &ledger,
                    &remote.node_id,
                    &provider_id,
                );
                session
                    .send(&NetMessage::JobResult {
                        job_id: result.job_id.clone(),
                        ok: result.ok,
                        text: result.text.clone(),
                        tokens: result.tokens,
                        error: result.error.clone(),
                    })
                    .await?;
            }
            NetMessage::Hello { capability } => {
                peers
                    .lock()
                    .expect("peers")
                    .insert(capability.node_id.clone(), capability);
            }
            NetMessage::JobResult { .. } => {
                // Ignore unexpected results on the worker.
            }
        }
    }
    Ok(())
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
) -> p2p_core::JobResult {
    let sealed = match net_to_sealed(ciphertext, ephemeral_pubkey) {
        Ok(s) => s,
        Err(e) => return p2p_core::JobResult::failure(job_id, e.to_string()),
    };
    let request = match open_job_request(&sealed, keypair) {
        Ok(r) => r,
        Err(e) => return p2p_core::JobResult::failure(job_id, e.to_string()),
    };
    // Prefer wire job_id if present; fall back to plaintext.
    let mut request = request;
    if request.job_id.is_empty() {
        request.job_id = job_id.to_string();
    }
    let result = engine.infer(&request);
    if result.ok {
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

pub async fn send_sealed_job(
    session: &mut PeerSession,
    request: &JobRequest,
    recipient: &NodeId,
) -> Result<p2p_core::JobResult, RuntimeError> {
    use p2p_crypto::seal_job_request;

    let sealed = seal_job_request(request, recipient)?;
    let msg = crate::wire::sealed_to_net(&request.job_id, &sealed)?;
    session.send(&msg).await?;
    match session.recv().await? {
        NetMessage::JobResult {
            job_id,
            ok,
            text,
            tokens,
            error,
        } => Ok(p2p_core::JobResult {
            job_id,
            ok,
            text,
            tokens,
            error,
        }),
        other => Err(RuntimeError::Protocol(format!(
            "expected JobResult, got {other:?}"
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
