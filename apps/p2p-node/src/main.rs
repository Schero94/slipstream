// `p2p-node` — thin Slipstream P2P CLI (standalone; does not touch Tauri release paths).
//
// Commands: `keygen`, `serve`/`start`, `peers`, `send-job`/`job`, `credits`,
// v0.6 "Use your other Mac": `share`, `use`, `peer-id` (NodeMode::Paired),
// M2: `pair --show-code` / `pair --peer <addr> --code <6 digits>`, `trust list|revoke`,
// `reset-identity --confirm`. M3: LAN discovery (`discover`, `share` announces
// `_slipstream._udp`, `use` finds the pinned Mac by fingerprint, `pair --peer <fp>`,
// `pair --show-code` opens the window inside a running share via `<data_dir>/share.sock`).
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
use p2p_node::pairing::{local_info, start_pair_listener, PairingWindow, SystemClock};
use p2p_node::trust::{NodeTrust, TrustStore};
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
    /// the Macs in this Mac's trust store (`slipstream-node pair`). Never spawns a model and
    /// never donates capacity to the community mesh.
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
        /// Name shown to paired Macs (default: this Mac's ComputerName).
        #[arg(long)]
        name: Option<String>,
        /// Write the pinned dial address (`.../p2p/<id>`) here once listening.
        #[arg(long)]
        addr_file: Option<PathBuf>,
        /// QUIC idle timeout in ms (peer-loss detection; default 8000).
        #[arg(long)]
        idle_timeout_ms: Option<u64>,
        /// Do not announce this Mac on the local network (mDNS `_slipstream._udp`). Paired
        /// Macs then need its address (they still try the last one that worked).
        #[arg(long, default_value_t = false)]
        no_announce: bool,
        /// PID of the llama-server process behind --forward. Requests are forwarded only
        /// while that process (or a child of it) owns the listening port; anything else
        /// listening there gets nothing (503). Without it, the listener must at least run
        /// under your user account.
        #[arg(long, value_name = "PID")]
        forward_pid: Option<u32>,
        #[arg(long, default_value_t = false)]
        verbose: bool,
    },
    /// List Macs on this network that share their model (mDNS), with their fingerprint,
    /// whether they are paired with this Mac and whether they show a pairing code.
    Discover {
        /// How long to listen, in ms.
        #[arg(long, default_value_t = 3000)]
        timeout_ms: u64,
        #[arg(long, env = "SLIPSTREAM_P2P_DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Use another Mac's model: serve it to editors on this Mac at --http (v0.6, headless).
    Use {
        /// Which paired Mac: its name, PeerId, or an address ending in `/p2p/<PeerId>` of a
        /// paired Mac (e.g. after its IP changed). Default: the only Mac that shares with
        /// this one.
        #[arg(long)]
        peer: Option<String>,
        /// Local editor address (loopback). Editors keep `http://127.0.0.1:<port>/v1`.
        #[arg(long, default_value = "127.0.0.1:8080")]
        http: SocketAddr,
        /// Node identity directory (node.key). Same default as the app.
        #[arg(long, env = "SLIPSTREAM_P2P_DIR")]
        data_dir: Option<PathBuf>,
        /// QUIC idle timeout in ms (peer-loss detection; default 8000).
        #[arg(long)]
        idle_timeout_ms: Option<u64>,
        /// Editors must send `Authorization: Bearer <token>`. The token is read from this
        /// file, or created there (32 random bytes as hex, mode 0600) if missing. Default:
        /// `<data dir>/local.token` (the same key the app writes into editor configs).
        /// Put it into the editor's "API key" field.
        #[arg(long, value_name = "PATH")]
        token_file: Option<PathBuf>,
        /// Do not require a token (any program on this Mac can then use the other Mac's
        /// model through --http). Not recommended.
        #[arg(long, default_value_t = false, conflicts_with = "token_file")]
        no_token: bool,
        /// Measurement only: do not keep a pre-opened spare stream.
        #[arg(long, default_value_t = false, hide = true)]
        no_spare_stream: bool,
        /// Keep running even if the paired Mac cannot be reached at start (editors get a
        /// 503 until it answers; Wake-on-LAN is tried). Default: exit after ~6 s.
        #[arg(long, default_value_t = false)]
        keep_trying: bool,
        /// Do not look for the paired Mac on the local network; dial only its stored
        /// address (or --peer).
        #[arg(long, default_value_t = false)]
        no_discovery: bool,
        #[arg(long, default_value_t = false)]
        verbose: bool,
    },
    /// Print this node's PeerId.
    PeerId {
        #[arg(long, env = "SLIPSTREAM_P2P_DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Pair two of your own Macs with a 6-digit code (v0.6 M2).
    ///
    /// On the Mac that shares its model: `pair --show-code` (prints the code and waits up to
    /// 5 minutes; if `share` is running with the same data dir, the window opens inside it).
    /// On the other Mac: `pair --peer <fingerprint from discover | address> --code <6 digits>`.
    Pair {
        /// Share side: open a pairing window and print the code.
        #[arg(long, default_value_t = false, conflicts_with_all = ["peer", "code"])]
        show_code: bool,
        /// Client side: the sharing Mac's fingerprint (from `discover`, ≥ 4 hex chars) or
        /// its address (printed by `pair --show-code`).
        #[arg(long, requires = "code")]
        peer: Option<String>,
        /// Client side: the 6-digit code shown on the other Mac.
        #[arg(long, requires = "peer")]
        code: Option<String>,
        /// Name the other Mac will show for this one (default: ComputerName).
        #[arg(long)]
        name: Option<String>,
        /// Share side: QUIC listen address when no share is running.
        #[arg(long, default_value = "/ip4/0.0.0.0/udp/9003/quic-v1")]
        listen: Multiaddr,
        /// Share side: do not let the paired Mac start/stop this Mac's model.
        #[arg(long, default_value_t = false)]
        no_model_control: bool,
        #[arg(long, env = "SLIPSTREAM_P2P_DIR")]
        data_dir: Option<PathBuf>,
    },
    /// List or remove paired Macs.
    Trust {
        #[command(subcommand)]
        action: TrustAction,
        #[arg(long, env = "SLIPSTREAM_P2P_DIR", global = true)]
        data_dir: Option<PathBuf>,
    },
    /// Advanced: give this Mac a new identity. Removes ALL pairings; every other Mac must
    /// pair again. Requires --confirm.
    ResetIdentity {
        /// Yes, create a new identity and remove every pairing.
        #[arg(long, default_value_t = false)]
        confirm: bool,
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

#[derive(Debug, Subcommand)]
enum TrustAction {
    /// Show the paired Macs.
    List,
    /// Remove a paired Mac (by name or PeerId). A running share/use drops it within ~0.25 s.
    Revoke {
        /// Name or PeerId.
        peer: String,
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
            name,
            addr_file,
            idle_timeout_ms,
            no_announce,
            forward_pid,
            verbose,
        } => {
            cmd_share(
                listen,
                forward,
                data_dir,
                name,
                addr_file,
                idle_timeout_ms,
                !no_announce,
                forward_pid,
                verbose,
            )
            .await?
        }
        Commands::Discover { timeout_ms, data_dir } => cmd_discover(timeout_ms, data_dir).await?,
        Commands::Use {
            peer,
            http,
            data_dir,
            idle_timeout_ms,
            token_file,
            no_token,
            no_spare_stream,
            no_discovery,
            keep_trying,
            verbose,
        } => {
            cmd_use(
                peer,
                http,
                data_dir,
                idle_timeout_ms,
                token_file,
                no_token,
                !no_spare_stream,
                !no_discovery,
                keep_trying,
                verbose,
            )
            .await?
        }
        Commands::PeerId { data_dir } => {
            let identity = p2p_forward::load_or_create_identity(&resolve_data_dir(data_dir)?)?;
            println!("{}", identity.public().to_peer_id());
        }
        Commands::Pair {
            show_code,
            peer,
            code,
            name,
            listen,
            no_model_control,
            data_dir,
        } => cmd_pair(show_code, peer, code, name, listen, !no_model_control, data_dir).await?,
        Commands::Trust { action, data_dir } => cmd_trust(action, data_dir)?,
        Commands::ResetIdentity { confirm, data_dir } => {
            if !confirm {
                return Err("reset-identity gives this Mac a new identity and removes ALL pairings; \
                    every other Mac must pair again. Re-run with --confirm to do it."
                    .into());
            }
            let dir = resolve_data_dir(data_dir)?;
            let node = p2p_node::trust::reset_identity(&dir)?;
            println!(
                "new identity peer_id={} — all pairings removed; restart share/use and pair again",
                p2p_forward::transport_identity(&node).public().to_peer_id()
            );
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

/// `--data-dir` / `SLIPSTREAM_P2P_DIR`, else the app's directory. Without HOME there is
/// no safe default (a shared `/tmp` directory could be pre-created by another user), so
/// refuse instead.
fn resolve_data_dir(data_dir: Option<PathBuf>) -> Result<PathBuf, String> {
    resolve_data_dir_from(data_dir, std::env::var_os("HOME"))
}

fn resolve_data_dir_from(
    data_dir: Option<PathBuf>,
    home: Option<std::ffi::OsString>,
) -> Result<PathBuf, String> {
    if let Some(dir) = data_dir {
        return Ok(dir);
    }
    match home.filter(|h| !h.is_empty()) {
        Some(home) => Ok(PathBuf::from(home).join("Library/Application Support/Slipstream/p2p")),
        None => Err("HOME is not set: pass --data-dir (or SLIPSTREAM_P2P_DIR) for the node identity".into()),
    }
}

fn paired_config(idle_timeout_ms: Option<u64>) -> p2p_libp2p::paired::PairedConfig {
    let mut cfg = p2p_libp2p::paired::PairedConfig::default();
    if let Some(ms) = idle_timeout_ms {
        cfg.idle_timeout = std::time::Duration::from_millis(ms.max(1000));
    }
    cfg
}

/// Runs until Ctrl-C / SIGTERM / SIGHUP, so callers can clean up (mDNS goodbye, control
/// socket) by returning normally.
async fn wait_for_ctrl_c() {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut hup)) = (signal(SignalKind::terminate()), signal(SignalKind::hangup())) else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
        _ = hup.recv() => {}
    }
}

#[allow(clippy::too_many_arguments)]
async fn cmd_share(
    listen: Multiaddr,
    forward: String,
    data_dir: Option<PathBuf>,
    name: Option<String>,
    addr_file: Option<PathBuf>,
    idle_timeout_ms: Option<u64>,
    announce: bool,
    forward_pid: Option<u32>,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let policy = NodePolicy::for_mode(NodeMode::Paired);
    policy.validate()?;
    let upstream = p2p_forward::parse_upstream(&forward)?;
    let dir = resolve_data_dir(data_dir)?;
    // Same session code as the app (p2p_node::session), headless: no model control.
    let node = p2p_node::session::Node::open(&dir)?;
    p2p_node::trust::share_requires_pairing(&node.store)?;
    let mut opts = p2p_node::session::ShareOpts::new(upstream);
    opts.listen = Some(listen.to_string());
    opts.name = name;
    // claimed at pairing via `pair --show-code` into this share: "may start/stop" (the
    // headless share refuses StartModel/StopModel anyway)
    opts.may_start_model = true;
    opts.announce = announce;
    opts.paired = paired_config(idle_timeout_ms);
    // Review M3: who may receive forwarded requests on --forward.
    opts.upstream_owner = match forward_pid {
        Some(pid) => p2p_forward::upstream::UpstreamOwner::fixed_pid(pid),
        None => {
            eprintln!(
                "share: forwarding only while the listener on {upstream} runs under your user account; pass --forward-pid <llama-server PID> to pin the exact process"
            );
            p2p_forward::upstream::UpstreamOwner::SameUid
        }
    };
    opts.verbose = verbose;
    let session = p2p_node::session::ShareSession::start(node.clone(), opts).await?;
    let dial = session.handle.dial_addr();
    println!(
        "sharing mode={} peer_id={} dial={dial} paired_macs={} fp={} announced={}",
        policy.mode,
        session.handle.peer_id,
        node.store.list().len(),
        p2p_node::lan::fingerprint(&session.handle.peer_id),
        session.announced
    );
    println!("on a paired Mac: slipstream-node use");
    if let Some(path) = addr_file {
        std::fs::write(&path, format!("{dial}\n"))?;
    }
    wait_for_ctrl_c().await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_use(
    peer: Option<String>,
    http: SocketAddr,
    data_dir: Option<PathBuf>,
    idle_timeout_ms: Option<u64>,
    token_file: Option<PathBuf>,
    no_token: bool,
    spare_stream: bool,
    discovery: bool,
    keep_trying: bool,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use p2p_node::session::UseState;
    let policy = NodePolicy::for_mode(NodeMode::Paired);
    policy.validate()?;
    let dir = resolve_data_dir(data_dir)?;
    // Same session code as the app (p2p_node::session): editor port bound at once, 503 while
    // the other Mac is away, Wake-on-LAN + re-dial, version check, "removed" detection.
    let node = p2p_node::session::Node::open(&dir)?;
    // Pinned dialing only: the target comes from the trust store; LAN discovery (M3) adds
    // the addresses announced under its fingerprint, always dialed as that PeerId.
    let (peer_id, addr) = if discovery {
        p2p_node::trust::resolve_use_peer(&node.store, peer.as_deref())?
    } else {
        let (id, a) = p2p_node::trust::resolve_use_target(&node.store, peer.as_deref())?;
        (id, Some(a))
    };
    // Review M2: a token by default (the same file the app uses), opt-out with --no-token.
    let token_file = match (token_file, no_token) {
        (Some(p), _) => Some(p),
        (None, false) => Some(dir.join("local.token")),
        (None, true) => {
            eprintln!("use: --no-token: any program on this Mac can use the other Mac's model through {http}");
            None
        }
    };
    let token = match &token_file {
        Some(path) => Some(p2p_forward::load_or_create_token(path)?),
        None => None,
    };
    let mut opts = p2p_node::session::UseOpts::new(peer_id.to_string(), http);
    opts.addr = addr;
    opts.token = token;
    opts.discovery = discovery;
    opts.paired = paired_config(idle_timeout_ms);
    opts.spare_stream = spare_stream;
    opts.verbose = verbose;
    let session = p2p_node::session::UseSession::start(node.clone(), opts)?;
    // The first connection must come within the dial budget (unless --keep-trying); later
    // the session rides out sleeps and network changes by itself.
    let first_deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    let mut announced = false;
    let mut last_state = String::new();
    let ctrl_c = wait_for_ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        let st = session.state();
        let line = format!("{st:?}");
        if line != last_state {
            if announced || verbose {
                eprintln!("use event=state state={}", p2p_forward::allowlist::display_safe(&line));
            }
            last_state = line;
        }
        match &st {
            UseState::Connected if !announced => {
                announced = true;
                match session.last_status() {
                    Some(s) => println!(
                        "using \"{}\" (Slipstream {}) model={} running={} n_ctx={} busy={} on_battery={}",
                        p2p_forward::allowlist::display_safe(&s.name),
                        p2p_forward::allowlist::display_safe(&s.app_version),
                        p2p_forward::allowlist::display_safe(s.model_id.as_deref().unwrap_or("-")),
                        s.model_running,
                        s.n_ctx.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
                        s.busy,
                        s.on_battery
                    ),
                    None => eprintln!("status unavailable"),
                }
                if let Some(h) = session.handle() {
                    println!("editors: http://{}/v1 (also http://{}/v1)", h.bind, h.bind_companion);
                }
                if let Some(path) = &token_file {
                    println!("editors must send the API key from {} (Authorization: Bearer)", path.display());
                }
            }
            UseState::Removed => {
                return Err(format!("\"{}\" removed this Mac from its paired Macs; pair again to use it", session.name).into())
            }
            UseState::VersionMismatch { side, version } => {
                let msg = p2p_node::session::version_answer(&session.name, side, version).message;
                return Err(msg.into());
            }
            UseState::PortBusy { error } => return Err(error.clone().into()),
            _ if !announced && !keep_trying && std::time::Instant::now() > first_deadline => {
                let why = session
                    .handle()
                    .and_then(|h| h.last_error())
                    .or(match &st {
                        UseState::Unreachable { error } | UseState::Asleep { error } => Some(error.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "dial timed out (is the other Mac awake and sharing?)".into());
                return Err(format!("could not reach the paired Mac: {why}").into());
            }
            _ => {}
        }
        tokio::select! {
            _ = &mut ctrl_c => return Ok(()),
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
        }
    }
}

async fn cmd_pair(
    show_code: bool,
    peer: Option<String>,
    code: Option<String>,
    name: Option<String>,
    listen: Multiaddr,
    may_start_model: bool,
    data_dir: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write as _;
    let dir = resolve_data_dir(data_dir)?;
    let node = p2p_forward::load_or_create_node(&dir)?;
    let store = TrustStore::load(&dir, &node)?;
    let paired = p2p_libp2p::paired::PairedConfig::default();
    if show_code {
        // A share running with this data dir owns the UDP port: open the window inside it.
        if let Some(mut ctl) = p2p_node::control::open_pairing_via_share(&dir).await? {
            if !may_start_model {
                eprintln!("note: the running share decides model control; restart it to change that");
            }
            return pair_via_running_share(&mut ctl).await;
        }
        let trust = NodeTrust::new(store.clone(), node.clone());
        let window = PairingWindow::open(Arc::new(SystemClock), store.epoch());
        let listener =
            start_pair_listener(trust, listen.clone(), &paired, p2p_node::pairing::share_info(name, may_start_model), window.clone()).await?;
        let code = window.code().as_str().to_string();
        let fp = p2p_node::lan::fingerprint(&listener.peer_id);
        // Announce with pair=1 so the other Mac finds it without an address.
        let _announcer = match p2p_node::lan::udp_port(&listener.listen_addr) {
            Some(port) if p2p_node::lan::should_announce(&listen) => {
                p2p_node::lan::start_share_announcer(listener.peer_id, port, listener.ctx.trust.clone())
                    .map_err(|e| eprintln!("not announced on the local network: {e}"))
                    .ok()
            }
            _ => None,
        };
        println!("pairing code: {} {}", &code[..3], &code[3..]);
        println!("code={code}");
        println!("dial={}", listener.dial_addr());
        println!("fp={fp}");
        println!("on the other Mac: slipstream-node pair --peer {fp} --code {code}");
        println!("   (or with the address: --peer {})", listener.dial_addr());
        println!("waiting up to 5 minutes (3 wrong codes close the window)...");
        std::io::stdout().flush()?;
        let state = window.wait_closed().await;
        match state {
            p2p_node::pairing::WindowState::Paired => {
                let p = window.paired_peer().expect("paired peer recorded");
                println!("paired with \"{}\" peer_id={}", p.name, p.peer_id);
                // let the Done message drain
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                Ok(())
            }
            p2p_node::pairing::WindowState::TooManyFailures => {
                Err("pairing closed after 3 wrong codes; run `pair --show-code` again for a new code".into())
            }
            p2p_node::pairing::WindowState::Expired => {
                Err("the pairing code expired (5 minutes); run `pair --show-code` again".into())
            }
            other => Err(format!("pairing window closed: {other:?}").into()),
        }
    } else {
        let (Some(peer), Some(code)) = (peer, code) else {
            return Err("use `pair --show-code` on the sharing Mac, or `pair --peer <addr> --code <6 digits>` here".into());
        };
        let code = p2p_crypto::PairingCode::parse(&code).map_err(|e| e.to_string())?;
        let t0 = std::time::Instant::now();
        let peer = peer.trim().to_string();
        let (targets, expect_fp): (Vec<Multiaddr>, Option<String>) = if peer.starts_with('/') {
            (vec![peer.parse::<Multiaddr>().map_err(|e| format!("bad --peer address: {e}"))?], None)
        } else if p2p_node::lan::looks_like_fp(&peer) {
            let (fp, addrs) = p2p_node::lan::resolve_fp(&peer, std::time::Duration::from_secs(5)).await?;
            (addrs, Some(fp))
        } else {
            return Err("--peer takes a fingerprint from `slipstream-node discover` (hex) or an address (/ip4/...)".into());
        };
        let entry = p2p_node::pairing::pair_with_checked(
            &node,
            &store,
            &targets,
            &code,
            local_info(name, false),
            &paired,
            expect_fp.as_deref(),
        )
        .await
        .map_err(|e| e.to_string())?;
        println!(
            "paired with \"{}\" (Slipstream {}) peer_id={} in {:.0} ms",
            entry.name,
            entry.app_version,
            entry.peer_id,
            t0.elapsed().as_secs_f64() * 1e3
        );
        println!("now: slipstream-node use");
        Ok(())
    }
}

/// `pair --show-code` while `share` runs: the share opens the window (control socket).
async fn pair_via_running_share(ctl: &mut p2p_node::control::ControlClient) -> Result<(), Box<dyn std::error::Error>> {
    use p2p_node::control::ControlReply;
    use std::io::Write as _;
    match ctl.next().await? {
        Some(ControlReply::Opened { code, dial, fp, remaining_s }) => {
            println!("pairing code: {} {}", &code[..3], &code[3..]);
            println!("code={code}");
            println!("dial={dial}");
            println!("fp={fp}");
            println!("(the running share opened the window)");
            println!("on the other Mac: slipstream-node pair --peer {fp} --code {code}");
            println!("waiting up to {} minutes (3 wrong codes close the window)...", remaining_s.div_ceil(60));
            std::io::stdout().flush()?;
        }
        Some(ControlReply::Error { message }) => return Err(message.into()),
        other => return Err(format!("unexpected reply from the running share: {other:?}").into()),
    }
    match ctl.next().await? {
        Some(ControlReply::Closed { state, name, peer_id }) => match state.as_str() {
            "paired" => {
                println!(
                    "paired with \"{}\" peer_id={}",
                    name.unwrap_or_default(),
                    peer_id.unwrap_or_default()
                );
                Ok(())
            }
            "too_many_failures" => Err("pairing closed after 3 wrong codes; run `pair --show-code` again for a new code".into()),
            "expired" => Err("the pairing code expired (5 minutes); run `pair --show-code` again".into()),
            s => Err(format!("pairing window closed: {s}").into()),
        },
        Some(other) => Err(format!("unexpected reply from the running share: {other:?}").into()),
        None => Err("the share stopped while the pairing window was open".into()),
    }
}

async fn cmd_discover(timeout_ms: u64, data_dir: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let dir = resolve_data_dir(data_dir)?;
    let node = p2p_forward::load_or_create_node(&dir)?;
    let store = TrustStore::load(&dir, &node)?;
    let found = p2p_node::lan::discover(Some(&store), std::time::Duration::from_millis(timeout_ms), None).await?;
    if found.is_empty() {
        println!(
            "no sharing Mac found on this network within {:.1} s (Not listed? Use its address: --peer /ip4/<ip>/udp/9003/quic-v1)",
            timeout_ms as f64 / 1e3
        );
        return Ok(());
    }
    println!("fp\tpaired\tpairing\taddresses");
    for d in found {
        let addrs: Vec<String> = d.addrs.iter().map(|ip| format!("{ip}:{}", d.port)).collect();
        println!(
            "{}\t{}\t{}\t{}",
            d.fp,
            match &d.paired {
                Some(p) => format!("paired \"{}\"", p2p_forward::allowlist::display_safe(&p.name)),
                None => "not paired".into(),
            },
            if d.pairing_open { "open" } else { "-" },
            addrs.join(",")
        );
        if d.pairing_open && d.paired.is_none() {
            println!("  pair: slipstream-node pair --peer {} --code <6 digits shown there>", d.fp);
        }
    }
    Ok(())
}

fn cmd_trust(action: TrustAction, data_dir: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let dir = resolve_data_dir(data_dir)?;
    let node = p2p_forward::load_or_create_node(&dir)?;
    let store = TrustStore::load(&dir, &node)?;
    match action {
        TrustAction::List => {
            if store.pairings_reset() {
                println!("warning: pairings were reset (trusted_macs.json failed its integrity check); pair again");
            }
            let peers = store.list();
            if peers.is_empty() {
                println!("no paired Macs");
            }
            for p in peers {
                println!(
                    "{}\t{}\t{}\tSlipstream {}\tmay_start_model={}\tpaired_at={}\t{}",
                    p.name,
                    p.peer_id,
                    p.role,
                    p.app_version,
                    p.may_start_model,
                    p.paired_at,
                    p.last_addrs.first().map(String::as_str).unwrap_or("-")
                );
            }
        }
        TrustAction::Revoke { peer } => {
            let removed = store.revoke(&peer).map_err(|e| e.to_string())?;
            println!("removed \"{}\" peer_id={}", removed.name, removed.peer_id);
        }
    }
    Ok(())
}

#[cfg(test)]
mod data_dir_tests {
    use super::*;

    /// M1 security review finding 7: no `/tmp` fallback without HOME.
    #[test]
    fn data_dir_requires_home_or_explicit_dir() {
        assert!(resolve_data_dir_from(None, None).is_err());
        assert!(resolve_data_dir_from(None, Some("".into())).is_err());
        assert_eq!(
            resolve_data_dir_from(None, Some("/Users/a".into())).unwrap(),
            PathBuf::from("/Users/a/Library/Application Support/Slipstream/p2p")
        );
        assert_eq!(
            resolve_data_dir_from(Some("/x".into()), None).unwrap(),
            PathBuf::from("/x")
        );
    }
}
