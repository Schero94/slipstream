//! `p2p-node` — thin Slipstream P2P CLI (standalone; does not touch Tauri release paths).
//!
//! Commands: `keygen`, `serve`/`start`, `peers`, `send-job`/`job`, `credits`.
//!
//! Engine selection (default **mock**):
//! - `--engine mock|auto|mlx|llama` or env `SLIPSTREAM_P2P_ENGINE`
//! - `--dry-run-engine` prints the exact argv that would be launched (no spawn)
//! - `--spawn-engine` starts that process (requires `--features launch`)

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use p2p_core::{JobRequest, NodeId};
use p2p_crypto::NodeKeypair;
use p2p_engine::{plan_serve_for_choice, resolve_engine_choice, EngineChoice};
use p2p_ledger::Ledger;
use p2p_node::{
    capability_for_engine, capability_to_advert, client_hello, default_capability, send_sealed_job,
    NodeConfig, RunningNode,
};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "p2p-node",
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
        /// Spawn the planned engine process before listening (needs `--features launch`).
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
            let choice = resolve_serve_engine(engine.as_deref(), real_engine, mock)?;
            if dry_run_engine {
                cmd_dry_run_engine(choice)?;
                return Ok(());
            }
            cmd_serve(
                listen,
                key,
                ledger,
                bootstrap,
                models,
                choice,
                spawn_engine,
            )
            .await?;
        }
        Commands::Peers { addrs, key, models } => cmd_peers(addrs, key, models).await?,
        Commands::SendJob {
            peer,
            key,
            prompt,
            system,
            model,
            max_tokens,
            job_id,
        } => cmd_send_job(peer, key, prompt, system, model, max_tokens, job_id).await?,
        Commands::Credits {
            ledger,
            account,
            job_id,
        } => cmd_credits(ledger, account, job_id)?,
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
    println!(
        "launch_feature={}",
        p2p_engine::launch_feature_enabled()
    );
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
    key: PathBuf,
    ledger: Option<PathBuf>,
    bootstrap: String,
    models: String,
    engine: EngineChoice,
    spawn_engine: bool,
) -> Result<(), Box<dyn std::error::Error>> {
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
    })?;
    node.serve().await?;
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
        match client_hello(addr, ours.clone()).await {
            Ok((_session, remote)) => {
                println!(
                    "peer addr={addr} id={} backend={} models={:?} ram_gib={} vram_gib={}",
                    remote.node_id, remote.backend, remote.models, remote.ram_gib, remote.vram_gib
                );
            }
            Err(e) => eprintln!("peer addr={addr} error={e}"),
        }
    }
    Ok(())
}

async fn cmd_send_job(
    peer: SocketAddr,
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
    let (mut session, remote) = client_hello(peer, ours).await?;
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
        "sending sealed job_id={job_id} to {} via {peer}",
        recipient.as_hex()
    );
    let result = send_sealed_job(&mut session, &request, &recipient).await?;
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
