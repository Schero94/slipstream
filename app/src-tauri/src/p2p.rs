//! Slipstream P2P thin adapter (feature-flagged from the UI via `slipstream.p2p`).
//!
//! Completely separate from Metal / MLX `start_server` paths. Uses `p2p-node`
//! runtime helpers with `EngineChoice::Mock` by default so no real oMLX/llama
//! is required. Optional `engine` on [`p2p_start`] selects stubs (`mlx`/`llama`/
//! `auto`); non-mock engines HTTP-infer against local Slipstream (`:8080`).
//! Process spawn stays behind CLI `--features launch`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use p2p_core::{JobRequest, NodeId};
use p2p_crypto::NodeKeypair;
use p2p_ledger::Ledger;
use p2p_node::{
    capability_for_engine, capability_to_advert, client_hello, default_capability, send_sealed_job,
    EngineChoice, NodeConfig, RunningNode,
};
use serde::{Deserialize, Serialize};
use tauri::State;

const DEFAULT_LISTEN: &str = "127.0.0.1:9002";
const DEFAULT_HTTP_INFER: &str = "http://127.0.0.1:8080";
const RECENT_PEERS_MAX: usize = 16;
const TEST_JOB_MAX_TOKENS: u32 = 8;
const CHAT_JOB_MAX_TOKENS: u32 = 64;

/// Managed state for the optional in-process P2P listener.
#[derive(Default)]
pub struct P2pState {
    inner: Mutex<Option<P2pRuntime>>,
    last_engine: Mutex<String>,
    last_job_id: Mutex<Option<String>>,
    recent_peers: Mutex<Vec<P2pPeerInfo>>,
}

struct P2pRuntime {
    node_id: String,
    listen_addr: String,
    engine: String,
    stop: tokio::sync::watch::Sender<bool>,
    _thread: thread::JoinHandle<()>,
}

#[derive(Debug, Clone, Serialize)]
pub struct P2pStatus {
    pub running: bool,
    pub node_id: String,
    pub listen_addr: String,
    pub credits: u64,
    pub mock: bool,
    /// Selected engine label: `mock` | `auto` | `mlx` | `llama`.
    pub engine: String,
    pub last_job_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct P2pJobOutcome {
    pub ok: bool,
    pub job_id: String,
    pub text: String,
    pub tokens: u32,
    pub error: Option<String>,
    pub peer_node_id: String,
    pub peer_addr: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct P2pPeerInfo {
    pub addr: String,
    pub ok: bool,
    pub node_id: String,
    pub backend: String,
    pub models: Vec<String>,
    pub ram_gib: u32,
    pub vram_gib: u32,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct P2pSettlementView {
    pub job_id: String,
    pub consumer_id: String,
    pub provider_id: String,
    pub tokens: u64,
    pub credits: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct P2pCredits {
    pub account: String,
    pub balance: u64,
    /// Back-compat alias (`credits` == `balance`).
    pub credits: u64,
    pub node_id: String,
    pub settlement: Option<P2pSettlementView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct RecentPeersFile {
    peers: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct JobParams {
    prompt: String,
    model: String,
    system: String,
    max_tokens: u32,
    job_id: Option<String>,
}

fn data_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Library/Application Support/Slipstream/p2p");
    }
    PathBuf::from("/tmp/slipstream-p2p")
}

fn ensure_dir(path: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| format!("p2p data dir: {e}"))
}

/// Load or create the persistent node key under Application Support.
pub fn load_or_create_keypair() -> Result<(NodeKeypair, PathBuf), String> {
    let dir = data_dir();
    ensure_dir(&dir)?;
    let key_path = dir.join("node.key");
    if key_path.is_file() {
        let kp = NodeKeypair::load(&key_path).map_err(|e| format!("load key: {e}"))?;
        return Ok((kp, key_path));
    }
    let kp = NodeKeypair::generate();
    kp.save(&key_path).map_err(|e| format!("save key: {e}"))?;
    Ok((kp, key_path))
}

fn ledger_path() -> PathBuf {
    data_dir().join("ledger.db")
}

fn recent_peers_path() -> PathBuf {
    data_dir().join("recent_peers.json")
}

fn read_credits(node_id: &str) -> u64 {
    let path = ledger_path();
    if !path.is_file() {
        return 0;
    }
    Ledger::open_sqlite(&path)
        .ok()
        .and_then(|l| l.balance(node_id).ok())
        .unwrap_or(0)
}

fn load_recent_addrs() -> Vec<String> {
    let path = recent_peers_path();
    if !path.is_file() {
        return vec![];
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<RecentPeersFile>(&s).ok())
        .map(|f| f.peers)
        .unwrap_or_default()
}

fn remember_peer_addr(addr: &str) {
    let addr = addr.trim();
    if addr.is_empty() {
        return;
    }
    let mut peers = load_recent_addrs();
    peers.retain(|p| p != addr);
    peers.insert(0, addr.to_string());
    peers.truncate(RECENT_PEERS_MAX);
    let _ = ensure_dir(&data_dir());
    if let Ok(json) = serde_json::to_string_pretty(&RecentPeersFile { peers }) {
        let _ = std::fs::write(recent_peers_path(), json);
    }
}

fn push_recent_peer(state: &P2pState, info: P2pPeerInfo) {
    remember_peer_addr(&info.addr);
    if let Ok(mut g) = state.recent_peers.lock() {
        g.retain(|p| p.addr != info.addr);
        g.insert(0, info);
        g.truncate(RECENT_PEERS_MAX);
    }
}

fn parse_engine_opt(engine: Option<String>) -> Result<EngineChoice, String> {
    match engine {
        None => Ok(EngineChoice::Mock),
        Some(s) if s.trim().is_empty() => Ok(EngineChoice::Mock),
        Some(s) => EngineChoice::parse(&s),
    }
}

fn parse_addrs_csv(csv: &str) -> Result<Vec<SocketAddr>, String> {
    let mut out = Vec::new();
    for part in csv.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        out.push(
            part.parse()
                .map_err(|e| format!("invalid addr '{part}': {e}"))?,
        );
    }
    Ok(out)
}

/// Ensure non-mock engines target local Slipstream HTTP (:8080) when unset.
fn ensure_http_infer_env(choice: EngineChoice) {
    if choice.is_mock() {
        return;
    }
    if std::env::var_os("P2P_MLX_ENDPOINT").is_none()
        && std::env::var_os("SLIPSTREAM_MLX_ENDPOINT").is_none()
    {
        std::env::set_var("P2P_MLX_ENDPOINT", DEFAULT_HTTP_INFER);
    }
    if matches!(choice, EngineChoice::Llama | EngineChoice::Auto)
        && std::env::var_os("P2P_LLAMA_ENDPOINT").is_none()
    {
        std::env::set_var("P2P_LLAMA_ENDPOINT", DEFAULT_HTTP_INFER);
    }
}

fn last_engine_of(state: &P2pState) -> String {
    state
        .last_engine
        .lock()
        .map(|g| {
            if g.is_empty() {
                "mock".into()
            } else {
                g.clone()
            }
        })
        .unwrap_or_else(|_| "mock".into())
}

fn last_job_of(state: &P2pState) -> Option<String> {
    state.last_job_id.lock().ok().and_then(|g| g.clone())
}

fn set_last_job(state: &P2pState, job_id: &str) {
    if let Ok(mut g) = state.last_job_id.lock() {
        *g = Some(job_id.to_string());
    }
}

fn status_from(
    runtime: Option<&P2pRuntime>,
    kp: &NodeKeypair,
    last_engine: &str,
    last_job_id: Option<String>,
) -> P2pStatus {
    let node_id = kp.node_id().as_hex().to_string();
    match runtime {
        Some(rt) => P2pStatus {
            running: true,
            node_id: rt.node_id.clone(),
            listen_addr: rt.listen_addr.clone(),
            credits: read_credits(&rt.node_id),
            mock: rt.engine == "mock",
            engine: rt.engine.clone(),
            last_job_id,
        },
        None => P2pStatus {
            running: false,
            credits: read_credits(&node_id),
            node_id,
            listen_addr: String::new(),
            mock: last_engine == "mock",
            engine: last_engine.to_string(),
            last_job_id,
        },
    }
}

fn running_listen_addr(state: &P2pState) -> Option<String> {
    state
        .inner
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|rt| rt.listen_addr.clone()))
}

/// Snapshot: node id / credits always available; listen only while running.
#[tauri::command]
pub fn p2p_status(state: State<P2pState>) -> Result<P2pStatus, String> {
    let (kp, _) = load_or_create_keypair()?;
    let guard = state
        .inner
        .lock()
        .map_err(|_| "p2p state poisoned".to_string())?;
    Ok(status_from(
        guard.as_ref(),
        &kp,
        &last_engine_of(&state),
        last_job_of(&state),
    ))
}

/// Credits balance (+ optional settlement by `job_id`).
/// `account` defaults to this node's persistent id.
#[tauri::command]
pub fn p2p_credits(
    account: Option<String>,
    job_id: Option<String>,
) -> Result<P2pCredits, String> {
    let (kp, _) = load_or_create_keypair()?;
    let node_id = kp.node_id().as_hex().to_string();
    let account = account
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| node_id.clone());
    let path = ledger_path();
    let balance = if path.is_file() {
        Ledger::open_sqlite(&path)
            .and_then(|l| l.balance(&account))
            .unwrap_or(0)
    } else {
        0
    };
    let settlement = job_id
        .filter(|s| !s.trim().is_empty())
        .and_then(|jid| {
            if !path.is_file() {
                return None;
            }
            Ledger::open_sqlite(&path)
                .ok()
                .and_then(|l| l.get_settlement(&jid).ok())
                .flatten()
                .map(|s| P2pSettlementView {
                    job_id: s.job_id,
                    consumer_id: s.consumer_id,
                    provider_id: s.provider_id,
                    tokens: s.tokens,
                    credits: s.credits,
                })
        });
    Ok(P2pCredits {
        account: account.clone(),
        balance,
        credits: balance,
        node_id,
        settlement,
    })
}

/// Start the P2P listener. Does not touch Metal/MLX server state.
///
/// `engine`: optional `mock` (default) | `auto` | `mlx` | `llama`.
/// `bootstrap`: optional comma-separated peer addrs dialed at bind time.
/// Non-mock → HTTP infer to local Slipstream (`P2P_MLX_ENDPOINT` / `:8080`).
/// Does **not** spawn oMLX/llama-server from the UI path.
#[tauri::command]
pub fn p2p_start(
    listen: String,
    engine: Option<String>,
    bootstrap: Option<String>,
    state: State<P2pState>,
) -> Result<P2pStatus, String> {
    let choice = parse_engine_opt(engine)?;
    ensure_http_infer_env(choice);
    let listen_raw = if listen.trim().is_empty() {
        DEFAULT_LISTEN.to_string()
    } else {
        listen.trim().to_string()
    };
    let listen_addr: SocketAddr = listen_raw
        .parse()
        .map_err(|e| format!("invalid listen addr '{listen_raw}': {e}"))?;
    let bootstrap_addrs = parse_addrs_csv(bootstrap.as_deref().unwrap_or(""))?;

    let mut guard = state
        .inner
        .lock()
        .map_err(|_| "p2p state poisoned".to_string())?;
    if guard.is_some() {
        let (kp, _) = load_or_create_keypair()?;
        return Ok(status_from(
            guard.as_ref(),
            &kp,
            &last_engine_of(&state),
            last_job_of(&state),
        ));
    }

    let (kp, _) = load_or_create_keypair()?;
    let node_id = kp.node_id().as_hex().to_string();
    let ledger_for_node = ledger_path();
    ensure_dir(ledger_for_node.parent().unwrap_or(std::path::Path::new(".")))?;

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<String, String>>();

    let keypair = Arc::new(kp);
    let engine_label = choice.as_str().to_string();
    if let Ok(mut g) = state.last_engine.lock() {
        *g = engine_label.clone();
    }
    for addr in &bootstrap_addrs {
        remember_peer_addr(&addr.to_string());
    }

    let thread = thread::Builder::new()
        .name("slipstream-p2p".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .thread_name("slipstream-p2p-worker")
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("tokio runtime: {e}")));
                    return;
                }
            };
            rt.block_on(async move {
                let models = if choice.is_mock() {
                    vec!["mock".into()]
                } else {
                    vec!["slipstream".into()]
                };
                let capability =
                    capability_for_engine(default_capability(models, choice.is_mock()), choice);
                let mut node = match RunningNode::open(NodeConfig {
                    listen: listen_addr,
                    keypair: Arc::clone(&keypair),
                    capability,
                    engine: choice,
                    spawn_engine: false,
                    ledger_path: Some(ledger_for_node),
                    bootstrap: bootstrap_addrs,
                }) {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("open node: {e}")));
                        return;
                    }
                };
                let (listener, addr) = match node.bind().await {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("bind: {e}")));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(addr.to_string()));
                let accept = tokio::spawn(async move {
                    let _ = node.accept_loop(listener).await;
                });
                let mut stop_rx = stop_rx;
                loop {
                    if *stop_rx.borrow() {
                        break;
                    }
                    if stop_rx.changed().await.is_err() {
                        break;
                    }
                }
                accept.abort();
            });
        })
        .map_err(|e| format!("spawn p2p thread: {e}"))?;

    let bound = ready_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "p2p start timed out".to_string())??;

    *guard = Some(P2pRuntime {
        node_id: node_id.clone(),
        listen_addr: bound,
        engine: engine_label,
        stop: stop_tx,
        _thread: thread,
    });

    let (kp, _) = load_or_create_keypair()?;
    Ok(status_from(
        guard.as_ref(),
        &kp,
        &last_engine_of(&state),
        last_job_of(&state),
    ))
}

/// Stop the in-process listener (Metal/MLX unchanged).
#[tauri::command]
pub fn p2p_stop(state: State<P2pState>) -> Result<P2pStatus, String> {
    let mut guard = state
        .inner
        .lock()
        .map_err(|_| "p2p state poisoned".to_string())?;
    if let Some(rt) = guard.take() {
        let _ = rt.stop.send(true);
        drop(rt);
    }
    let (kp, _) = load_or_create_keypair()?;
    Ok(status_from(
        None,
        &kp,
        &last_engine_of(&state),
        last_job_of(&state),
    ))
}

/// Dial Hello against comma-separated peer addrs (capability probe).
#[tauri::command]
pub fn p2p_peers(addrs: String, state: State<P2pState>) -> Result<Vec<P2pPeerInfo>, String> {
    let parsed = parse_addrs_csv(&addrs)?;
    if parsed.is_empty() {
        return Err("provide at least one peer address".into());
    }
    let (kp, _) = load_or_create_keypair()?;
    let cap = default_capability(vec!["mock".into()], true);
    let ours = capability_to_advert(&kp.node_id(), &cap, true);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("slipstream-p2p-peers")
        .build()
        .map_err(|e| format!("tokio: {e}"))?;

    let out = rt.block_on(async move {
        let mut out = Vec::with_capacity(parsed.len());
        for addr in parsed {
            match client_hello(addr, ours.clone()).await {
                Ok((_session, remote)) => out.push(P2pPeerInfo {
                    addr: addr.to_string(),
                    ok: true,
                    node_id: remote.node_id,
                    backend: remote.backend,
                    models: remote.models,
                    ram_gib: remote.ram_gib,
                    vram_gib: remote.vram_gib,
                    error: None,
                }),
                Err(e) => out.push(P2pPeerInfo {
                    addr: addr.to_string(),
                    ok: false,
                    node_id: String::new(),
                    backend: String::new(),
                    models: vec![],
                    ram_gib: 0,
                    vram_gib: 0,
                    error: Some(e.to_string()),
                }),
            }
        }
        out
    });
    for info in out.iter().filter(|p| p.ok) {
        push_recent_peer(&state, info.clone());
    }
    Ok(out)
}

/// Recent peers (in-memory probe cache + persisted addrs).
#[tauri::command]
pub fn p2p_recent_peers(state: State<P2pState>) -> Result<Vec<P2pPeerInfo>, String> {
    let cached = state
        .recent_peers
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    if !cached.is_empty() {
        return Ok(cached);
    }
    Ok(load_recent_addrs()
        .into_iter()
        .map(|addr| P2pPeerInfo {
            addr,
            ok: false,
            node_id: String::new(),
            backend: String::new(),
            models: vec![],
            ram_gib: 0,
            vram_gib: 0,
            error: None,
        })
        .collect())
}

fn normalize_prompt(prompt: Option<String>, fallback: &str) -> String {
    let p = prompt.unwrap_or_default().trim().to_string();
    if p.is_empty() {
        fallback.to_string()
    } else {
        p
    }
}

fn resolve_job_params(
    prompt: String,
    model: Option<String>,
    system: Option<String>,
    max_tokens: Option<u32>,
    job_id: Option<String>,
    default_max: u32,
    default_model: &str,
) -> JobParams {
    let model = model
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default_model.to_string());
    JobParams {
        prompt,
        model,
        system: system.unwrap_or_default(),
        max_tokens: max_tokens.unwrap_or(default_max).max(1),
        job_id: job_id.filter(|s| !s.trim().is_empty()),
    }
}

fn require_chat_prompt(prompt: String) -> Result<String, String> {
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() {
        return Err("prompt required".into());
    }
    Ok(prompt)
}

/// Resolve empty/loopback peer to the running node listen addr when available.
fn resolve_peer_target(peer: &str, state: &P2pState) -> PeerTarget {
    let trimmed = peer.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("loopback") {
        if let Some(addr) = running_listen_addr(state) {
            return PeerTarget::Addr(addr);
        }
        return PeerTarget::EphemeralLoopback;
    }
    PeerTarget::Addr(trimmed.to_string())
}

enum PeerTarget {
    EphemeralLoopback,
    Addr(String),
}

fn run_p2p_job(peer: String, params: JobParams, state: &P2pState) -> Result<P2pJobOutcome, String> {
    let target = resolve_peer_target(&peer, state);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("slipstream-p2p-job")
        .build()
        .map_err(|e| format!("tokio: {e}"))?;

    rt.block_on(async move {
        match target {
            PeerTarget::EphemeralLoopback => send_loopback_job(&params).await,
            PeerTarget::Addr(addr_str) => {
                let addr: SocketAddr = addr_str
                    .parse()
                    .map_err(|e| format!("invalid peer '{addr_str}': {e}"))?;
                send_job_to(addr, &params).await
            }
        }
    })
}

/// Seal + send a job to `peer` (host:port). Empty peer → running node, else ephemeral loopback.
#[tauri::command]
pub fn p2p_send_test_job(
    peer: String,
    prompt: Option<String>,
    model: Option<String>,
    system: Option<String>,
    max_tokens: Option<u32>,
    job_id: Option<String>,
    state: State<P2pState>,
) -> Result<P2pJobOutcome, String> {
    let prompt = normalize_prompt(prompt, "hello slipstream p2p");
    let params = resolve_job_params(
        prompt,
        model,
        system,
        max_tokens,
        job_id,
        TEST_JOB_MAX_TOKENS,
        "mock",
    );
    let out = run_p2p_job(peer, params, &state)?;
    if out.ok {
        set_last_job(&state, &out.job_id);
        remember_peer_addr(&out.peer_addr);
    }
    Ok(out)
}

/// Chat-oriented P2P ask: prompt required, optional peer (empty = running node / loopback).
#[tauri::command]
pub fn p2p_chat(
    prompt: String,
    peer: Option<String>,
    model: Option<String>,
    system: Option<String>,
    max_tokens: Option<u32>,
    job_id: Option<String>,
    state: State<P2pState>,
) -> Result<P2pJobOutcome, String> {
    let prompt = require_chat_prompt(prompt)?;
    let default_model = if last_engine_of(&state) == "mock" {
        "mock"
    } else {
        "slipstream"
    };
    let params = resolve_job_params(
        prompt,
        model,
        system,
        max_tokens,
        job_id,
        CHAT_JOB_MAX_TOKENS,
        default_model,
    );
    let out = run_p2p_job(peer.unwrap_or_default(), params, &state)?;
    if out.ok {
        set_last_job(&state, &out.job_id);
        remember_peer_addr(&out.peer_addr);
    }
    Ok(out)
}

async fn send_job_to(addr: SocketAddr, params: &JobParams) -> Result<P2pJobOutcome, String> {
    // Persistent client identity — same key as this Mac's provider node.
    let (client_kp, _) = load_or_create_keypair()?;
    let cap = default_capability(vec![params.model.clone()], true);
    let ours = capability_to_advert(&client_kp.node_id(), &cap, true);
    let (mut session, remote) = client_hello(addr, ours)
        .await
        .map_err(|e| format!("hello: {e}"))?;
    let job_id = params
        .job_id
        .clone()
        .unwrap_or_else(|| format!("ui-{}", chrono_like_id()));
    let job = JobRequest {
        job_id: job_id.clone(),
        model: params.model.clone(),
        system: params.system.clone(),
        prompt: params.prompt.clone(),
        max_tokens: params.max_tokens.max(1),
    };
    let recipient = NodeId::from_hex(&remote.node_id).map_err(|e| format!("peer id: {e}"))?;
    let result = send_sealed_job(&mut session, &job, &recipient, &client_kp)
        .await
        .map_err(|e| format!("job: {e}"))?;
    remember_peer_addr(&addr.to_string());
    Ok(P2pJobOutcome {
        ok: result.ok,
        job_id: result.job_id,
        text: result.text,
        tokens: result.tokens,
        error: result.error,
        peer_node_id: remote.node_id,
        peer_addr: addr.to_string(),
    })
}

/// Ephemeral mock worker on `127.0.0.1:0` so Ask works without a started node.
async fn send_loopback_job(params: &JobParams) -> Result<P2pJobOutcome, String> {
    let dir = tempfile_dir()?;
    let ledger = dir.join("loopback.db");
    let worker_kp = Arc::new(NodeKeypair::generate());
    let mut node = RunningNode::open(NodeConfig {
        listen: "127.0.0.1:0"
            .parse()
            .map_err(|e| format!("listen parse: {e}"))?,
        keypair: Arc::clone(&worker_kp),
        capability: default_capability(vec![params.model.clone()], true),
        engine: EngineChoice::Mock,
        spawn_engine: false,
        ledger_path: Some(ledger),
        bootstrap: vec![],
    })
    .map_err(|e| format!("open: {e}"))?;
    let (listener, addr) = node.bind().await.map_err(|e| format!("bind: {e}"))?;
    let accept = tokio::spawn(async move {
        let _ = node.accept_loop(listener).await;
    });
    let outcome = send_job_to(addr, params).await;
    accept.abort();
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

fn tempfile_dir() -> Result<PathBuf, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let base = std::env::temp_dir().join(format!("slipstream-p2p-{}-{}", chrono_like_id(), n));
    std::fs::create_dir_all(&base).map_err(|e| format!("temp dir: {e}"))?;
    Ok(base)
}

fn chrono_like_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{ms}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_params(prompt: &str, max_tokens: u32) -> JobParams {
        JobParams {
            prompt: prompt.into(),
            model: "mock".into(),
            system: String::new(),
            max_tokens,
            job_id: None,
        }
    }

    #[tokio::test]
    async fn loopback_sealed_job_ok() {
        let out = send_loopback_job(&sample_params("hello mesh", TEST_JOB_MAX_TOKENS))
            .await
            .expect("loopback");
        assert!(out.ok, "{out:?}");
        assert!(out.tokens > 0);
        assert!(!out.peer_node_id.is_empty());
    }

    #[tokio::test]
    async fn loopback_chat_job_ok() {
        let out = send_loopback_job(&sample_params("what is slipstream?", CHAT_JOB_MAX_TOKENS))
            .await
            .expect("chat loopback");
        assert!(out.ok, "{out:?}");
        assert!(out.tokens > TEST_JOB_MAX_TOKENS);
        assert!(out.text.contains("[mock"));
        assert!(!out.peer_node_id.is_empty());
    }

    #[tokio::test]
    async fn loopback_uses_persistent_client_key() {
        let (kp, _) = load_or_create_keypair().expect("key");
        let local_id = kp.node_id().as_hex().to_string();
        let a = send_loopback_job(&sample_params("a", 4)).await.expect("a");
        let b = send_loopback_job(&sample_params("b", 4)).await.expect("b");
        assert!(a.ok && b.ok, "{a:?} {b:?}");
        assert_ne!(a.peer_node_id, local_id);
        assert_ne!(b.peer_node_id, local_id);
    }

    #[tokio::test]
    async fn job_fields_model_system_job_id_roundtrip() {
        let params = JobParams {
            prompt: "ping".into(),
            model: "mock".into(),
            system: "be brief".into(),
            max_tokens: 6,
            job_id: Some("ui-job-fields-1".into()),
        };
        let out = send_loopback_job(&params).await.expect("job");
        assert!(out.ok, "{out:?}");
        assert_eq!(out.job_id, "ui-job-fields-1");
        assert_eq!(out.tokens, 6);
    }

    #[test]
    fn peers_hello_against_ephemeral_worker() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("rt");
        let dir = tempfile_dir().expect("dir");
        let addr = rt.block_on(async {
            let worker_kp = Arc::new(NodeKeypair::generate());
            let mut node = RunningNode::open(NodeConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                keypair: Arc::clone(&worker_kp),
                capability: default_capability(vec!["mock".into()], true),
                engine: EngineChoice::Mock,
                spawn_engine: false,
                ledger_path: Some(dir.join("peers.db")),
                bootstrap: vec![],
            })
            .expect("open");
            let (listener, addr) = node.bind().await.expect("bind");
            tokio::spawn(async move {
                let _ = node.accept_loop(listener).await;
            });
            addr
        });
        // Probe without Tauri State: exercise parse + hello path via client_hello.
        let list = rt.block_on(async {
            let (kp, _) = load_or_create_keypair().unwrap();
            let cap = default_capability(vec!["mock".into()], true);
            let ours = capability_to_advert(&kp.node_id(), &cap, true);
            let (_s, remote) = client_hello(addr, ours).await.expect("hello");
            vec![P2pPeerInfo {
                addr: addr.to_string(),
                ok: true,
                node_id: remote.node_id,
                backend: remote.backend,
                models: remote.models,
                ram_gib: remote.ram_gib,
                vram_gib: remote.vram_gib,
                error: None,
            }]
        });
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(list.len(), 1);
        assert!(list[0].ok, "{:?}", list[0]);
        assert_eq!(list[0].node_id.len(), 64);
        assert_eq!(list[0].backend, "mock");
    }

    #[test]
    fn chat_prompt_rejects_empty() {
        let err = require_chat_prompt("   ".into()).expect_err("empty");
        assert!(err.contains("prompt"), "{err}");
    }

    #[test]
    fn parse_bootstrap_csv() {
        let v = parse_addrs_csv("127.0.0.1:9001, 127.0.0.1:9002").expect("ok");
        assert_eq!(v.len(), 2);
        assert!(parse_addrs_csv("").unwrap().is_empty());
        assert!(parse_addrs_csv("not-an-addr").is_err());
    }

    #[test]
    fn parse_engine_opt_defaults_mock() {
        assert!(parse_engine_opt(None).unwrap().is_mock());
        assert_eq!(parse_engine_opt(Some("mlx".into())).unwrap().as_str(), "mlx");
    }

    #[test]
    fn keypair_persists_under_data_dir() {
        let (kp, path) = load_or_create_keypair().expect("key");
        assert_eq!(kp.node_id().as_hex().len(), 64);
        assert!(path.is_file());
        let (kp2, _) = load_or_create_keypair().expect("reload");
        assert_eq!(kp.node_id().as_hex(), kp2.node_id().as_hex());
    }

    #[test]
    fn credits_view_shape() {
        let (kp, _) = load_or_create_keypair().expect("key");
        let view = p2p_credits(Some(kp.node_id().as_hex().to_string()), None).expect("credits");
        assert_eq!(view.account, kp.node_id().as_hex());
        assert_eq!(view.balance, view.credits);
        assert!(view.settlement.is_none());
    }

    #[test]
    fn empty_peer_prefers_running_listen() {
        let state = P2pState::default();
        assert!(matches!(
            resolve_peer_target("", &state),
            PeerTarget::EphemeralLoopback
        ));
        {
            let mut g = state.inner.lock().unwrap();
            let (tx, _rx) = tokio::sync::watch::channel(false);
            *g = Some(P2pRuntime {
                node_id: "abc".into(),
                listen_addr: "127.0.0.1:9002".into(),
                engine: "mock".into(),
                stop: tx,
                _thread: thread::spawn(|| {}),
            });
        }
        match resolve_peer_target("", &state) {
            PeerTarget::Addr(a) => assert_eq!(a, "127.0.0.1:9002"),
            _ => panic!("expected Addr"),
        }
        let _ = state.inner.lock().unwrap().take();
    }

    #[test]
    fn remember_peer_persists() {
        remember_peer_addr("127.0.0.1:9555");
        assert!(load_recent_addrs().iter().any(|p| p == "127.0.0.1:9555"));
    }
}
