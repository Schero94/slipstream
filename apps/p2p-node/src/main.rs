// `p2p-node` — thin Slipstream P2P CLI (standalone; does not touch Tauri release paths).
//
// Commands: `keygen`, `serve`/`start`, `peers`, `send-job`/`job`, `credits`,
// v0.6 "Use your other Mac": `share`, `use`, `peer-id` (NodeMode::Paired).
//
// Engine selection (default **mock**):
// - `--engine mock|auto|mlx|llama` or env `SLIPSTREAM_P2P_ENGINE`
// - `--dry-run-engine` prints the exact argv that would be launched (no spawn)
// - `--spawn-engine` starts that process (requires `--features launch`);
//   **DANGER** — refuses if the oMLX/PGRN lock is live or `:8080` (configured
//   endpoint) already serves; dual heavy serve can freeze the Mac

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use libp2p::Multiaddr;
use p2p_core::{JobRequest, NodeId};
use p2p_crypto::NodeKeypair;
use p2p_engine::{plan_serve_for_choice, resolve_engine_choice, EngineChoice};
use p2p_ledger::Ledger;
use p2p_node::{
    capability_for_engine, capability_to_advert, client_hello, default_capability, send_sealed_job,
    mesh::{send_mesh_job, serve_mesh}, NodeConfig, NodeMode, NodePolicy, RunningNode,
};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "slipstream-node",
    about = "Slipstream P2P MVP CLI — start a node, list peers, send sealed jobs, show credits",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Generate an X25519 NodeKeypair and write it as hex to PATH.
    Keygen {
        /// Output path for the secret (64 hex chars + newline, mode 0600 on Unix).
        #[arg(short, long, default_value = "node.key")]
        out: PathBuf,
    },
    /// Start a listening worker node (Hello + sealed jobs + credit settle).
    #[command(visible_alias = "start")]
    Serve {
        /// Bind address (e.g. 127.0.0.1:9001).
        #[arg(long, default_value = "127.0.0.1:0")]
        listen: SocketAddr,
        /// Exposure mode: local | private | community.
        #[arg(long, default_value = "local")]
        mode: NodeMode,
        /// Donate bounded inference capacity. Valid only with --mode community.
        #[arg(long, default_value_t = false)]
        donate_capacity: bool,
        /// Path to NodeKeypair secret file (generated if missing).
        #[arg(long, default_value = "node.key")]
        key: PathBuf,
        /// SQLite ledger path (in-memory if omitted).
        #[arg(long)]
        ledger: Option<PathBuf>,
        /// Comma-separated bootstrap peers: `127.0.0.1:9001,127.0.0.1:9002`.
        #[arg(long, default_value = "")]
        bootstrap: String,
        /// Models to advertise (comma-separated).
        #[arg(long, default_value = "mock")]
        models: String,
        /// Force mock engine (CI / local demo; default on unless `--engine` / env set).
        #[arg(long, default_value_t = true)]
        mock: bool,
        /// Engine: `mock` | `auto` | `mlx` | `llama`.
        /// Also accepts env `SLIPSTREAM_P2P_ENGINE`. Overrides `--mock` when set.
        #[arg(long, value_name = "NAME")]
        engine: Option<String>,
        /// Deprecated alias for `--engine auto` (OS-selected stubs; no GPU spawn).
        #[arg(long, default_value_t = false)]
        real_engine: bool,
        /// Validate + print the exact engine serve argv, then exit (no listen / no spawn).
        #[arg(long, default_value_t = false)]
        dry_run_engine: bool,
        /// DANGER: spawn a heavy oMLX/llama serve (needs `--features launch`).
        /// Refuses if `/tmp/slipstream-omlx-pgrn.lock` holds a live pid, or if
        /// port 8080 / the configured endpoint already has a healthy
        /// Slipstream/llama/omlx serve. Dual serve can freeze the Mac — prefer
        /// attaching to an already-running Slipstream (omit this flag).
        #[arg(long, default_value_t = false)]
        spawn_engine: bool,
    },
    /// Serve sealed inference over direct libp2p QUIC.
    ///
    /// Traffic and payloads are encrypted in transit; the selected worker sees plaintext
    /// during inference. Community capacity donation remains explicit and bounded.
    MeshServe {
        /// QUIC listen multiaddress.
        #[arg(long, default_value = "/ip4/127.0.0.1/udp/0/quic-v1")]
        listen: Multiaddr,
        /// Exposure mode: local | private | community.
        #[arg(long, default_value = "local")]
        mode: NodeMode,
        /// Donate bounded inference capacity. Valid only with --mode community.
        #[arg(long, default_value_t = false)]
        donate_capacity: bool,
        /// Persistent node key (generated if missing).
        #[arg(long, default_value = "node.key")]
        key: PathBuf,
        /// Models to advertise (comma-separated).
        #[arg(long, default_value = "mock")]
        models: String,
        /// Engine: mock | auto | mlx | llama.
        #[arg(long, default_value = "mock")]
        engine: String,
        /// Optional SQLite ledger (community free mode does not settle credits).
        #[arg(long)]
        ledger: Option<PathBuf>,
        /// Spawn the selected heavy engine (requires the launch feature).
        #[arg(long, default_value_t = false)]
        spawn_engine: bool,
    },
    /// Dial peers, exchange Hello, print capabilities.
    Peers {
        /// Comma-separated peer addresses.
        #[arg(long)]
        addrs: String,
        /// Local key file (for node_id in Hello).
        #[arg(long, default_value = "node.key")]
        key: PathBuf,
        #[arg(long, default_value = "mock")]
        models: String,
    },
    /// Send an encrypted job to a peer and print the result.
    #[command(visible_alias = "job")]
    SendJob {
        /// Worker listen address.
        #[arg(long)]
        peer: SocketAddr,
        /// Optional pinned Ed25519 peer identity (64 lowercase hex chars).
        #[arg(long)]
        expected_peer_id: Option<String>,
        /// Local client key file (generated if missing).
        #[arg(long, default_value = "client.key")]
        key: PathBuf,
        /// Prompt text.
        #[arg(long, default_value = "hello mesh")]
        prompt: String,
        /// Optional system prompt.
        #[arg(long, default_value = "")]
        system: String,
        /// Model id (must be advertised by the worker).
        #[arg(long, default_value = "mock")]
        model: String,
        #[arg(long, default_value_t = 8)]
        max_tokens: u32,
        /// Optional job id (random if omitted).
        #[arg(long)]
        job_id: Option<String>,
    },
    /// Send one sealed job over direct libp2p QUIC.
    MeshSendJob {
        /// Worker QUIC multiaddress printed by mesh-serve.
        #[arg(long)]
        peer: Multiaddr,
        /// Optional pinned Ed25519 worker identity (64 lowercase hex chars).
        #[arg(long)]
        expected_peer_id: Option<String>,
        /// Persistent client key (generated if missing).
        #[arg(long, default_value = "client.key")]
        key: PathBuf,
        #[arg(long, default_value = "hello mesh")]
        prompt: String,
        #[arg(long, default_value = "")]
        system: String,
        #[arg(long, default_value = "mock")]
        model: String,
        #[arg(long, default_value_t = 8)]
        max_tokens: u32,
        #[arg(long)]
        job_id: Option<String>,
    },
    /// Share this Mac's running model with your own paired Macs (v0.6, headless).
    ///
    /// Forwards only the allowlisted OpenAI endpoints to the local llama-server (never
    /// `POST /slots/*` or `POST /props`), over an encrypted QUIC connection, and only for
    /// the peers named with --allow-peer (M1; pairing codes come in M2). Never spawns a
    /// model and never donates capacity to the community mesh.
    Share {
        /// QUIC listen multiaddress.
        #[arg(long, default_value = "/ip4/0.0.0.0/udp/9003/quic-v1")]
        listen: Multiaddr,
        /// The local llama-server to forward to (loopback only).
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        forward: String,
        /// Node identity directory (node.key). Same default as the app.
        #[arg(long, env = "SLIPSTREAM_P2P_DIR")]
        data_dir: Option<PathBuf>,
        /// PeerId allowed to use this Mac (repeat or comma-separate). Required.
        #[arg(long = "allow-peer", value_delimiter = ',', required = true)]
        allow_peer: Vec<libp2p::PeerId>,
        /// Name shown to paired Macs (default: this Mac's ComputerName).
        #[arg(long)]
        name: Option<String>,
        /// Write the pinned dial address (`.../p2p/<id>`) here once listening.
        #[arg(long)]
        addr_file: Option<PathBuf>,
        /// QUIC idle timeout in ms (peer-loss detection; default 8000).
        #[arg(long)]
        idle_timeout_ms: Option<u64>,
        #[arg(long, default_value_t = false)]
        verbose: bool,
    },
    /// Use another Mac's model: serve it to editors on this Mac at --http (v0.6, headless).
    Use {
        /// Pinned share address printed by `share`: `/ip4/<ip>/udp/9003/quic-v1/p2p/<PeerId>`.
        #[arg(long)]
        peer: Multiaddr,
        /// Local editor address (loopback). Editors keep `http://127.0.0.1:<port>/v1`.
        #[arg(long, default_value = "127.0.0.1:8080")]
        http: SocketAddr,
        /// Node identity directory (node.key). Same default as the app.
        #[arg(long, env = "SLIPSTREAM_P2P_DIR")]
        data_dir: Option<PathBuf>,
        /// QUIC idle timeout in ms (peer-loss detection; default 8000).
        #[arg(long)]
        idle_timeout_ms: Option<u64>,
        /// Measurement only: do not keep a pre-opened spare stream.
        #[arg(long, default_value_t = false, hide = true)]
        no_spare_stream: bool,
        #[arg(long, default_value_t = false)]
        verbose: bool,
    },
    /// Print this node's PeerId (give it to the sharing Mac's --allow-peer).
    PeerId {
        #[arg(long, env = "SLIPSTREAM_P2P_DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Show credit balances from a ledger file.
    Credits {
        /// SQLite ledger path used by the worker.
        #[arg(long)]
        ledger: PathBuf,
        /// Account id (node hex). Required to read a balance.
        #[arg(long)]
        account: Option<String>,
        /// Optional job_id to print settlement record.
        #[arg(long)]
        job_id: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let cli = Cli::parse();
    match cli.command {
        Commands::Keygen { out } => cmd_keygen(out)?,
        Commands::Serve {
            listen,
            mode,
            donate_capacity,
            key,
            ledger,
            bootstrap,
            models,
            mock,
            engine,
            real_engine,
            dry_run_engine,
            spawn_engine,
        } => {
            refuse_paired_for_job_api(mode)?;
            let choice = resolve_serve_engine(engine.as_deref(), real_engine, mock)?;
            if dry_run_engine {
                cmd_dry_run_engine(choice)?;
                return Ok(());
            }
            cmd_serve(
                listen,
                mode,
                donate_capacity,
                key,
                ledger,
                bootstrap,
                models,
                choice,
                spawn_engine,
            )
            .await?;
        }
        Commands::MeshServe {
            listen,
            mode,
            donate_capacity,
            key,
            models,
            engine,
            ledger,
            spawn_engine,
        } => {
            refuse_paired_for_job_api(mode)?;
            let choice = EngineChoice::parse(&engine)?;
            cmd_mesh_serve(
                listen,
                mode,
                donate_capacity,
                key,
                models,
                choice,
                ledger,
                spawn_engine,
            )
            .await?;
        }
        Commands::Peers { addrs, key, models } => cmd_peers(addrs, key, models).await?,
        Commands::SendJob {
            peer,
            expected_peer_id,
            key,
            prompt,
            system,
            model,
            max_tokens,
            job_id,
        } => {
            cmd_send_job(
                peer,
                expected_peer_id,
                key,
                prompt,
                system,
                model,
                max_tokens,
                job_id,
            )
            .await?
        }
        Commands::MeshSendJob {
            peer,
            expected_peer_id,
            key,
            prompt,
            system,
            model,
            max_tokens,
            job_id,
        } => {
            cmd_mesh_send_job(
                peer,
                expected_peer_id,
                key,
                prompt,
                system,
                model,
                max_tokens,
                job_id,
            )
            .await?
        }
        Commands::Credits {
            ledger,
            account,
            job_id,
        } => cmd_credits(ledger, account, job_id)?,
        Commands::Share {
            listen,
            forward,
            data_dir,
            allow_peer,
            name,
            addr_file,
            idle_timeout_ms,
            verbose,
        } => {
            cmd_share(
                listen,
                forward,
                data_dir,
                allow_peer,
                name,
                addr_file,
                idle_timeout_ms,
                verbose,
            )
            .await?
        }
        Commands::Use {
            peer,
            http,
            data_dir,
            idle_timeout_ms,
            no_spare_stream,
            verbose,
        } => cmd_use(peer, http, data_dir, idle_timeout_ms, !no_spare_stream, verbose).await?,
        Commands::PeerId { data_dir } => {
            let identity = p2p_forward::load_or_create_identity(&resolve_data_dir(data_dir))?;
            println!("{}", identity.public().to_peer_id());
        }
    }
    Ok(())
}

fn resolve_serve_engine(
    engine_cli: Option<&str>,
    real_engine: bool,
    mock: bool,
) -> Result<EngineChoice, Box<dyn std::error::Error>> {
    let cli = if let Some(s) = engine_cli {
        Some(EngineChoice::parse(s)?)
    } else if real_engine {
        Some(EngineChoice::Auto)
    } else {
        None
    };
    Ok(resolve_engine_choice(cli, mock)?)
}

fn cmd_dry_run_engine(choice: EngineChoice) -> Result<(), Box<dyn std::error::Error>> {
    if choice.is_mock() {
        return Err(
            "dry-run needs a real engine: pass --engine mlx|llama|auto (or SLIPSTREAM_P2P_ENGINE)"
                .into(),
        );
    }
    let os = std::env::consts::OS;
    let plan = plan_serve_for_choice(choice, os)?;
    println!("engine={}", choice.as_str());
    println!("backend={:?}", plan.backend);
    println!("program={}", plan.program.display());
    println!("argv={}", plan.argv().join(" "));
    println!("display={}", plan.display());
    println!("launch_feature={}", p2p_engine::launch_feature_enabled());
    println!("# spawn with: cargo run -p p2p-node --features launch -- serve --engine {} --spawn-engine …", choice.as_str());
    Ok(())
}

fn cmd_keygen(out: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let kp = NodeKeypair::generate();
    kp.save(&out)?;
    println!("wrote key {}", out.display());
    println!("node_id {}", kp.node_id());
    Ok(())
}

fn load_or_create_key(path: &PathBuf) -> Result<NodeKeypair, Box<dyn std::error::Error>> {
    if path.exists() {
        Ok(NodeKeypair::load(path)?)
    } else {
        let kp = NodeKeypair::generate();
        kp.save(path)?;
        eprintln!(
            "generated new key at {} (node_id {})",
            path.display(),
            kp.node_id()
        );
        Ok(kp)
    }
}

fn parse_models(csv: &str) -> Vec<String> {
    csv.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_addrs(csv: &str) -> Result<Vec<SocketAddr>, Box<dyn std::error::Error>> {
    let mut out = Vec::new();
    for part in csv.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        out.push(part.parse()?);
    }
    Ok(out)
}

async fn cmd_serve(
    listen: SocketAddr,
    mode: NodeMode,
    donate_capacity: bool,
    key: PathBuf,
    ledger: Option<PathBuf>,
    bootstrap: String,
    models: String,
    engine: EngineChoice,
    spawn_engine: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut policy = NodePolicy::for_mode(mode);
    policy.donate_capacity = donate_capacity;
    policy.validate_for_listen(listen)?;
    let keypair = Arc::new(load_or_create_key(&key)?);
    let capability = capability_for_engine(
        default_capability(parse_models(&models), engine.is_mock()),
        engine,
    );
    let bootstrap = parse_addrs(&bootstrap)?;
    let node = RunningNode::open(NodeConfig {
        listen,
        keypair: Arc::clone(&keypair),
        capability,
        engine,
        spawn_engine,
        ledger_path: ledger,
        bootstrap,
        policy,
    })?;
    node.serve().await?;
    Ok(())
}

async fn cmd_mesh_serve(
    listen: Multiaddr,
    mode: NodeMode,
    donate_capacity: bool,
    key: PathBuf,
    models: String,
    engine: EngineChoice,
    ledger: Option<PathBuf>,
    spawn_engine: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut policy = NodePolicy::for_mode(mode);
    policy.donate_capacity = donate_capacity;
    policy.validate()?;
    let keypair = Arc::new(load_or_create_key(&key)?);
    let capability = capability_for_engine(
        default_capability(parse_models(&models), engine.is_mock()),
        engine,
    );
    let node = RunningNode::open(NodeConfig {
        // The QUIC multiaddress is validated by serve_mesh. This loopback value
        // keeps the shared engine/policy runtime independent of TCP binding.
        listen: "127.0.0.1:0".parse()?,
        keypair,
        capability,
        engine,
        spawn_engine,
        ledger_path: ledger,
        bootstrap: Vec::new(),
        policy,
    })?;
    serve_mesh(node, listen).await?;
    Ok(())
}

async fn cmd_peers(
    addrs: String,
    key: PathBuf,
    models: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let keypair = load_or_create_key(&key)?;
    let cap = default_capability(parse_models(&models), true);
    let ours = capability_to_advert(&keypair.node_id(), &cap, true);
    let addrs = parse_addrs(&addrs)?;
    if addrs.is_empty() {
        return Err("provide --addrs with at least one peer".into());
    }
    println!("local node_id={}", keypair.node_id());
    for addr in addrs {
        match client_hello(addr, ours.clone(), &keypair, None).await {
            Ok((_session, remote)) => {
                println!(
                    "peer addr={addr} identity_id={} encryption_id={} backend={} models={:?} ram_gib={} vram_gib={}",
                    remote.identity_id,
                    remote.node_id,
                    remote.backend,
                    remote.models,
                    remote.ram_gib,
                    remote.vram_gib
                );
            }
            Err(e) => eprintln!("peer addr={addr} error={e}"),
        }
    }
    Ok(())
}

async fn cmd_send_job(
    peer: SocketAddr,
    expected_peer_id: Option<String>,
    key: PathBuf,
    prompt: String,
    system: String,
    model: String,
    max_tokens: u32,
    job_id: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let keypair = load_or_create_key(&key)?;
    let cap = default_capability(vec![model.clone()], true);
    let ours = capability_to_advert(&keypair.node_id(), &cap, true);
    let (mut session, remote) =
        client_hello(peer, ours, &keypair, expected_peer_id.as_deref()).await?;
    let recipient = NodeId::from_hex(&remote.node_id)?;
    let job_id = job_id.unwrap_or_else(|| format!("job-{}", &keypair.node_id().as_hex()[..8]));
    let request = JobRequest {
        job_id: job_id.clone(),
        model,
        system,
        prompt,
        max_tokens,
    };
    println!(
        "sending sealed job_id={job_id} to identity={} encryption={} via {peer}",
        remote.identity_id,
        recipient.as_hex(),
    );
    let result = send_sealed_job(&mut session, &request, &recipient, &keypair).await?;
    if result.ok {
        println!("ok tokens={} text={}", result.tokens, result.text);
    } else {
        println!(
            "fail error={}",
            result.error.unwrap_or_else(|| "unknown".into())
        );
        std::process::exit(1);
    }
    Ok(())
}

async fn cmd_mesh_send_job(
    peer: Multiaddr,
    expected_peer_id: Option<String>,
    key: PathBuf,
    prompt: String,
    system: String,
    model: String,
    max_tokens: u32,
    job_id: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let keypair = load_or_create_key(&key)?;
    let generated_id = || {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        format!("mesh-{}-{millis}", &keypair.node_id().as_hex()[..8])
    };
    let request = JobRequest {
        job_id: job_id.unwrap_or_else(generated_id),
        model,
        system,
        prompt,
        max_tokens,
    };
    eprintln!(
        "privacy: encrypted in transit; selected worker sees plaintext during inference"
    );
    let outcome = send_mesh_job(
        peer,
        &keypair,
        &request,
        expected_peer_id.as_deref(),
    )
    .await?;
    println!(
        "worker identity={} encryption={} transport={}",
        outcome.worker_identity, outcome.worker_encryption_id, outcome.transport_peer_id
    );
    if outcome.result.ok {
        println!(
            "ok tokens={} text={}",
            outcome.result.tokens, outcome.result.text
        );
    } else {
        println!(
            "fail error={}",
            outcome.result.error.unwrap_or_else(|| "unknown".into())
        );
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_credits(
    ledger_path: PathBuf,
    account: Option<String>,
    job_id: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let ledger = Ledger::open_sqlite(&ledger_path)?;
    println!("ledger={}", ledger_path.display());
    if let Some(ref job_id) = job_id {
        match ledger.get_settlement(job_id)? {
            Some(s) => println!(
                "settlement job_id={} consumer={} provider={} tokens={} credits={}",
                s.job_id, s.consumer_id, s.provider_id, s.tokens, s.credits
            ),
            None => println!("settlement job_id={job_id} not found"),
        }
    }
    if let Some(account) = account {
        let bal = ledger.balance(&account)?;
        println!("account={account} balance={bal}");
    } else if job_id.is_none() {
        println!("pass --account <node_id_hex> and/or --job-id <id>");
    }
    Ok(())
}

/// `--mode paired` belongs to `share` / `use` only (the job API has no allowlist).
fn refuse_paired_for_job_api(mode: NodeMode) -> Result<(), Box<dyn std::error::Error>> {
    if mode == NodeMode::Paired {
        return Err("--mode paired is only for `slipstream-node share` / `use` (your own Macs)".into());
    }
    Ok(())
}

/// `--data-dir` / `SLIPSTREAM_P2P_DIR`, else the app's directory.
fn resolve_data_dir(data_dir: Option<PathBuf>) -> PathBuf {
    data_dir.unwrap_or_else(|| match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join("Library/Application Support/Slipstream/p2p"),
        None => PathBuf::from("/tmp/slipstream-p2p"),
    })
}

fn paired_config(idle_timeout_ms: Option<u64>) -> p2p_libp2p::paired::PairedConfig {
    let mut cfg = p2p_libp2p::paired::PairedConfig::default();
    if let Some(ms) = idle_timeout_ms {
        cfg.idle_timeout = std::time::Duration::from_millis(ms.max(1000));
    }
    cfg
}

async fn wait_for_ctrl_c() {
    // Runs until Ctrl-C / SIGTERM (default signal action ends the process).
    std::future::pending::<()>().await;
}

#[allow(clippy::too_many_arguments)]
async fn cmd_share(
    listen: Multiaddr,
    forward: String,
    data_dir: Option<PathBuf>,
    allow_peer: Vec<libp2p::PeerId>,
    name: Option<String>,
    addr_file: Option<PathBuf>,
    idle_timeout_ms: Option<u64>,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let policy = NodePolicy::for_mode(NodeMode::Paired);
    policy.validate()?;
    let upstream = p2p_forward::parse_upstream(&forward)?;
    let dir = resolve_data_dir(data_dir);
    let identity = p2p_forward::load_or_create_identity(&dir)?;
    let handle = p2p_forward::start_share(
        identity,
        p2p_forward::ShareConfig {
            listen,
            upstream,
            allow: allow_peer,
            paired: paired_config(idle_timeout_ms),
            admission: p2p_forward::admission::AdmissionLimits::default(),
            name: name.unwrap_or_else(p2p_forward::status::default_name),
            verbose,
        },
    )
    .await?;
    let dial = handle.dial_addr();
    println!("sharing mode={} peer_id={} dial={dial}", policy.mode, handle.peer_id);
    println!("on the other Mac: slipstream-node use --peer {dial}");
    if let Some(path) = addr_file {
        std::fs::write(&path, format!("{dial}\n"))?;
    }
    wait_for_ctrl_c().await;
    Ok(())
}

async fn cmd_use(
    peer: Multiaddr,
    http: SocketAddr,
    data_dir: Option<PathBuf>,
    idle_timeout_ms: Option<u64>,
    spare_stream: bool,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let policy = NodePolicy::for_mode(NodeMode::Paired);
    policy.validate()?;
    let dir = resolve_data_dir(data_dir);
    let identity = p2p_forward::load_or_create_identity(&dir)?;
    let handle = p2p_forward::start_use(
        identity,
        p2p_forward::UseConfig {
            peer,
            bind: http,
            paired: paired_config(idle_timeout_ms),
            spare_stream,
            verbose,
        },
    )
    .await?;
    match handle.status().await {
        Ok(p2p_forward::status::StatusReply::Status(s)) => println!(
            "using \"{}\" (Slipstream {}) model={} running={} n_ctx={} busy={} on_battery={}",
            s.name,
            s.app_version,
            s.model_id.as_deref().unwrap_or("-"),
            s.model_running,
            s.n_ctx.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
            s.busy,
            s.on_battery
        ),
        Ok(other) => println!("status: {other:?}"),
        Err(e) => eprintln!("status unavailable: {e}"),
    }
    println!("editors: http://{}/v1", handle.bind);
    wait_for_ctrl_c().await;
    Ok(())
}
