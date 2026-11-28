//! `forward-spike` — v0.6 M0 transport spike CLI (bench/forward/run_m0.sh drives it).
//!
//!   forward-spike share --key-dir A --listen /ip4/127.0.0.1/udp/0/quic-v1 --upstream 127.0.0.1:18090 [--mode sealed]
//!   forward-spike use   --key-dir B --peer <addr>/p2p/<id> --bind 127.0.0.1:18091 [--mode sealed]
//!   forward-spike mock  --bind 127.0.0.1:18090
//!   forward-spike bench --arm direct=127.0.0.1:18090 --arm raw=127.0.0.1:18091 --n 200 --path '/mock?n=512'
//!   forward-spike aead
//!   forward-spike mock-llama --bind 127.0.0.1:18190          (M1 llama-server mock)
//!   forward-spike shaper --bind 127.0.0.1:19003 --target 127.0.0.1:9003 --rtt-ms 8   (M1 UDP shaper)
//!
//! Loopback only: the raw pipe has no allowlist / trust gate (M1/M2).

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use libp2p::Multiaddr;
use p2p_forward::bench::{aead_bench, percentile, ring_aead_bench, run_request, Sample};
use p2p_forward::node::{start_share, start_use};
use p2p_forward::pipe::PipeMode;
use p2p_forward::{load_or_create_identity, mock};
use p2p_libp2p::paired::PairedConfig;

#[path = "../../tests/support/udp_shaper.rs"]
mod udp_shaper;

#[derive(Parser)]
#[command(about = "Slipstream v0.6 M0 transport spike")]
struct Cli {
    /// QUIC max idle timeout in ms (peer-loss detection); default 8000.
    #[arg(long, global = true)]
    idle_timeout_ms: Option<u64>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Big-Mac side: accept pipe streams and connect each to --upstream.
    Share {
        #[arg(long)]
        key_dir: PathBuf,
        #[arg(long, default_value = "/ip4/127.0.0.1/udp/0/quic-v1")]
        listen: Multiaddr,
        #[arg(long)]
        upstream: SocketAddr,
        #[arg(long, default_value = "raw")]
        mode: PipeMode,
        /// Write the pinned dial address here once listening.
        #[arg(long)]
        addr_file: Option<PathBuf>,
        #[arg(long)]
        verbose: bool,
    },
    /// MacBook side: persistent connection to --peer, serve --bind.
    Use {
        #[arg(long)]
        key_dir: PathBuf,
        #[arg(long)]
        peer: Multiaddr,
        #[arg(long)]
        bind: SocketAddr,
        #[arg(long, default_value = "raw")]
        mode: PipeMode,
        #[arg(long)]
        verbose: bool,
    },
    /// Deterministic SSE mock upstream.
    Mock {
        #[arg(long)]
        bind: SocketAddr,
    },
    /// Interleaved client measurement over named arms.
    Bench {
        /// name=host:port (first arm is the baseline)
        #[arg(long = "arm", required = true)]
        arms: Vec<String>,
        #[arg(long, default_value_t = 200)]
        n: usize,
        #[arg(long, default_value = "/mock?n=512")]
        path: String,
        /// JSON body file → POST with Content-Type application/json
        #[arg(long)]
        body_file: Option<PathBuf>,
        #[arg(long, default_value_t = 0)]
        cooldown_ms: u64,
        /// Warm-up requests per arm (not recorded).
        #[arg(long, default_value_t = 3)]
        warmup: usize,
        /// Per-sample TSV output.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// M1 llama-server mock (SSE variants, tool calls, /slots with a secret prompt).
    MockLlama {
        #[arg(long)]
        bind: SocketAddr,
    },
    /// Userspace UDP shaper: fixed RTT (+ optional jitter / loss) in front of a QUIC port.
    Shaper {
        #[arg(long)]
        bind: SocketAddr,
        #[arg(long)]
        target: SocketAddr,
        #[arg(long, default_value_t = 8.0)]
        rtt_ms: f64,
        #[arg(long, default_value_t = 0.0)]
        jitter_ms: f64,
        #[arg(long, default_value_t = 0.0)]
        loss: f64,
    },
    /// AEAD cost per chunk (seal + open).
    Aead {
        #[arg(long, default_value_t = 200_000)]
        iters: usize,
    },
}

fn build_request(path: &str, host: SocketAddr, body: Option<&[u8]>) -> Vec<u8> {
    // The share side drops query strings (M1 security fixes); mock parameters travel in
    // a header too, so direct and forwarded arms get the same stream.
    let mock = path
        .split_once('?')
        .map(|(_, q)| format!("X-Mock-Params: {q}\r\n"))
        .unwrap_or_default();
    let mut req = match body {
        Some(b) => format!(
            "POST {path} HTTP/1.1\r\nHost: {host}\r\n{mock}Content-Type: application/json\r\nAccept: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            b.len()
        )
        .into_bytes(),
        None => format!(
            "POST {path} HTTP/1.1\r\nHost: {host}\r\n{mock}Content-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes(),
    };
    if let Some(b) = body {
        req.extend_from_slice(b);
    }
    req
}

fn summarize(name: &str, samples: &[Sample], base: Option<&[Sample]>) {
    let mut ttft: Vec<f64> = samples.iter().map(|s| s.ttft_us).collect();
    ttft.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut rate: Vec<f64> = samples.iter().map(|s| s.event_rate).collect();
    rate.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean_rate = rate.iter().sum::<f64>() / rate.len().max(1) as f64;
    let mut total: Vec<f64> = samples.iter().map(|s| s.total_us).collect();
    total.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let bodies: BTreeSet<_> = samples
        .iter()
        .map(|s| s.body_sha256[..16].to_string())
        .collect();
    let contents: BTreeSet<_> = samples
        .iter()
        .map(|s| s.content_sha256[..16].to_string())
        .collect();
    let statuses: BTreeSet<_> = samples.iter().map(|s| s.status_line.clone()).collect();
    println!(
        "arm={name} n={} ttft_us p50={:.0} p95={:.0} p99={:.0} | total_ms p50={:.2} | event_rate mean={:.1}/s median={:.1}/s | events={} body_len={} body_sha={:?} content_sha={:?} status={:?}",
        samples.len(),
        percentile(&ttft, 50.0),
        percentile(&ttft, 95.0),
        percentile(&ttft, 99.0),
        percentile(&total, 50.0) / 1e3,
        mean_rate,
        percentile(&rate, 50.0),
        samples.first().map(|s| s.events).unwrap_or(0),
        samples.first().map(|s| s.body_len).unwrap_or(0),
        bodies,
        contents,
        statuses,
    );
    if let Some(base) = base {
        let mut b_ttft: Vec<f64> = base.iter().map(|s| s.ttft_us).collect();
        b_ttft.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mut paired: Vec<f64> = samples
            .iter()
            .zip(base)
            .map(|(s, b)| s.ttft_us - b.ttft_us)
            .collect();
        paired.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let b_rate = base.iter().map(|s| s.event_rate).sum::<f64>() / base.len().max(1) as f64;
        println!(
            "  overhead vs baseline: ttft p50 {:+.0} us, p95 {:+.0} us (difference of percentiles); paired per-iteration delta p50 {:+.0} us, p95 {:+.0} us; event_rate {:+.2} %",
            percentile(&ttft, 50.0) - percentile(&b_ttft, 50.0),
            percentile(&ttft, 95.0) - percentile(&b_ttft, 95.0),
            percentile(&paired, 50.0),
            percentile(&paired, 95.0),
            (mean_rate / b_rate - 1.0) * 100.0,
        );
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let cli = Cli::parse();
    let mut config = PairedConfig::default();
    if let Some(ms) = cli.idle_timeout_ms {
        config.idle_timeout = Duration::from_millis(ms);
    }
    match cli.cmd {
        Cmd::Share {
            key_dir,
            listen,
            upstream,
            mode,
            addr_file,
            verbose,
        } => {
            let identity = load_or_create_identity(&key_dir)?;
            let handle = start_share(identity, listen, upstream, mode, &config, verbose).await?;
            let addr = handle.dial_addr();
            println!("share listening {addr} mode={mode:?} upstream={upstream}");
            if let Some(path) = addr_file {
                std::fs::write(path, addr.to_string())?;
            }
            std::future::pending::<()>().await;
            Ok(())
        }
        Cmd::Use {
            key_dir,
            peer,
            bind,
            mode,
            verbose,
        } => {
            let identity = load_or_create_identity(&key_dir)?;
            let handle = start_use(identity, peer, bind, mode, &config, verbose).await?;
            println!(
                "use serving {} → peer {} mode={mode:?}",
                handle.bind, handle.peer_id
            );
            let stats = handle.stats.clone();
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let s = stats.lock().unwrap();
                let mut opens = s.stream_open_us.clone();
                opens.sort_unstable();
                let mut rtts = s.ping_rtt_us.clone();
                rtts.sort_unstable();
                let p = |v: &[u64], q: f64| {
                    if v.is_empty() {
                        0
                    } else {
                        v[(((v.len() - 1) as f64) * q).round() as usize]
                    }
                };
                eprintln!(
                    "use stats streams={} stream_open_us p50={} p95={} ping_rtt_us p50={} p95={} conns est={} closed={}",
                    s.streams_piped,
                    p(&opens, 0.5),
                    p(&opens, 0.95),
                    p(&rtts, 0.5),
                    p(&rtts, 0.95),
                    s.connections_established,
                    s.connections_closed
                );
            }
        }
        Cmd::Mock { bind } => {
            let listener = mock::bind(bind).await?;
            println!("mock listening {}", listener.local_addr()?);
            mock::serve(listener).await
        }
        Cmd::Bench {
            arms,
            n,
            path,
            body_file,
            cooldown_ms,
            warmup,
            out,
        } => {
            let body = body_file.map(std::fs::read).transpose()?;
            let arms: Vec<(String, SocketAddr)> = arms
                .iter()
                .map(|a| {
                    let (name, addr) = a.split_once('=').expect("--arm name=host:port");
                    (name.to_string(), addr.parse().expect("socket address"))
                })
                .collect();
            let requests: Vec<Vec<u8>> = arms
                .iter()
                .map(|(_, addr)| build_request(&path, *addr, body.as_deref()))
                .collect();
            for _ in 0..warmup {
                for (i, (_, addr)) in arms.iter().enumerate() {
                    run_request(*addr, &requests[i]).await?;
                }
            }
            let mut samples: Vec<Vec<Sample>> = vec![Vec::new(); arms.len()];
            let mut tsv = String::from("iter\tarm\tttft_us\ttotal_us\tevents\tevent_rate\tbody_len\tbody_sha256\tcontent_sha256\n");
            for iter in 0..n {
                // rotate the arm order each iteration (A,B,C / B,C,A / ...) to cancel order effects
                for k in 0..arms.len() {
                    let i = (iter + k) % arms.len();
                    let s = run_request(arms[i].1, &requests[i]).await?;
                    tsv.push_str(&format!(
                        "{iter}\t{}\t{:.1}\t{:.1}\t{}\t{:.3}\t{}\t{}\t{}\n",
                        arms[i].0,
                        s.ttft_us,
                        s.total_us,
                        s.events,
                        s.event_rate,
                        s.body_len,
                        s.body_sha256,
                        s.content_sha256
                    ));
                    samples[i].push(s);
                    if cooldown_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(cooldown_ms)).await;
                    }
                }
            }
            if let Some(out) = out {
                std::fs::write(out, tsv)?;
            }
            for (i, (name, _)) in arms.iter().enumerate() {
                summarize(name, &samples[i], (i > 0).then(|| samples[0].as_slice()));
            }
            Ok(())
        }
        Cmd::MockLlama { bind } => {
            let m = p2p_forward::mock_llama::start(bind).await?;
            println!("mock-llama listening {}", m.addr);
            std::future::pending::<()>().await;
            Ok(())
        }
        Cmd::Shaper {
            bind,
            target,
            rtt_ms,
            jitter_ms,
            loss,
        } => {
            let s = udp_shaper::start_on(bind, target, Duration::from_secs_f64(rtt_ms / 1e3)).await?;
            s.control.set_jitter(Duration::from_secs_f64(jitter_ms / 1e3));
            s.control.set_loss(loss);
            println!("shaper {} -> {target} rtt_ms={rtt_ms} jitter_ms={jitter_ms} loss={loss}", s.addr);
            std::future::pending::<()>().await;
            Ok(())
        }
        Cmd::Aead { iters } => {
            for size in [64usize, 200, 512, 1024, 4096, 16 * 1024] {
                let r = aead_bench(size, iters);
                println!(
                    "aead chunk={}B seal_ns={:.0} open_ns={:.0} sum_ns={:.0} ({:.2} GB/s seal)",
                    r.chunk_bytes,
                    r.seal_ns,
                    r.open_ns,
                    r.seal_ns + r.open_ns,
                    r.chunk_bytes as f64 / r.seal_ns.max(1e-9)
                );
            }
            println!("raw ring 0.17 ChaCha20-Poly1305 (SealedStream's backend since M1; the lines above include its framing)");
            for size in [64usize, 200, 512, 1024, 4096, 16 * 1024] {
                let r = ring_aead_bench(size, iters);
                println!(
                    "ring chunk={}B seal_ns={:.0} open_ns={:.0} sum_ns={:.0} ({:.2} GB/s seal)",
                    r.chunk_bytes,
                    r.seal_ns,
                    r.open_ns,
                    r.seal_ns + r.open_ns,
                    r.chunk_bytes as f64 / r.seal_ns.max(1e-9)
                );
            }
            Ok(())
        }
    }
}
