//! Userspace UDP shaper for one-Mac tests (plan §7): sits between the use side and the
//! share side's QUIC port and adds a fixed one-way delay (RTT/2 each direction), optional
//! jitter and loss, and a blackhole switch (= the big Mac fell asleep / left the network).
//!
//! Packets of each direction go through one FIFO with a per-packet release time, so a
//! constant delay never reorders packets. Shared with `forward-spike shaper` via
//! `#[path]`, so the bench uses the same code as the tests.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

#[derive(Debug, Default)]
pub struct ShaperControl {
    one_way_us: AtomicU64,
    jitter_us: AtomicU64,
    loss_ppm: AtomicU64,
    blackhole: AtomicBool,
    pub forwarded: AtomicU64,
    pub dropped: AtomicU64,
}

impl ShaperControl {
    pub fn set_rtt(&self, rtt: Duration) {
        self.one_way_us
            .store(rtt.as_micros() as u64 / 2, Ordering::Relaxed);
    }
    pub fn set_jitter(&self, jitter: Duration) {
        self.jitter_us
            .store(jitter.as_micros() as u64, Ordering::Relaxed);
    }
    pub fn set_loss(&self, fraction: f64) {
        self.loss_ppm
            .store((fraction.clamp(0.0, 1.0) * 1e6) as u64, Ordering::Relaxed);
    }
    pub fn set_blackhole(&self, on: bool) {
        self.blackhole.store(on, Ordering::Relaxed);
    }
}

pub struct UdpShaper {
    /// Address the use side dials instead of the share side's QUIC port.
    pub addr: SocketAddr,
    pub control: Arc<ShaperControl>,
    tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Drop for UdpShaper {
    fn drop(&mut self) {
        for t in self.tasks.lock().unwrap().iter() {
            t.abort();
        }
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

type Packet = (Instant, Vec<u8>);

/// Per-direction delay line: decides drop/delay on entry, releases in FIFO order.
fn delay_line(
    control: Arc<ShaperControl>,
    seed: u64,
    send: impl Fn(Vec<u8>) + Send + 'static,
) -> mpsc::UnboundedSender<Vec<u8>> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (qtx, mut qrx) = mpsc::unbounded_channel::<Packet>();
    let entry = tokio::spawn(async move {
        let mut rng = Rng(seed | 1);
        let mut last_release = Instant::now();
        while let Some(pkt) = rx.recv().await {
            if control.blackhole.load(Ordering::Relaxed) {
                control.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let loss = control.loss_ppm.load(Ordering::Relaxed);
            if loss > 0 && rng.next() % 1_000_000 < loss {
                control.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let mut d = control.one_way_us.load(Ordering::Relaxed);
            let j = control.jitter_us.load(Ordering::Relaxed);
            if j > 0 {
                d += rng.next() % (j + 1);
            }
            // FIFO: never release before the previous packet (no reordering).
            let release = (Instant::now() + Duration::from_micros(d)).max(last_release);
            last_release = release;
            if qtx.send((release, pkt)).is_err() {
                break;
            }
        }
    });
    // Ends when the flow's sender is dropped (shaper dropped → tasks aborted).
    tokio::spawn(async move {
        while let Some((at, pkt)) = qrx.recv().await {
            tokio::time::sleep_until(at).await;
            send(pkt);
        }
        entry.abort();
    });
    tx
}

/// Start a shaper in front of `target` (the share side's UDP/QUIC port).
pub async fn start(target: SocketAddr, rtt: Duration) -> std::io::Result<UdpShaper> {
    start_on("127.0.0.1:0".parse().unwrap(), target, rtt).await
}

pub async fn start_on(bind: SocketAddr, target: SocketAddr, rtt: Duration) -> std::io::Result<UdpShaper> {
    let front = Arc::new(UdpSocket::bind(bind).await?);
    let addr = front.local_addr()?;
    let control = Arc::new(ShaperControl::default());
    control.set_rtt(rtt);
    let tasks: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
    let task_list = tasks.clone();
    let ctl = control.clone();
    let main = tokio::spawn(async move {
        let mut flows: HashMap<SocketAddr, mpsc::UnboundedSender<Vec<u8>>> = HashMap::new();
        let mut buf = vec![0u8; 65536];
        let mut seed = 0x1234_5678_9abc_def1u64;
        loop {
            let Ok((n, client)) = front.recv_from(&mut buf).await else {
                continue;
            };
            let up = match flows.get(&client) {
                Some(tx) => tx.clone(),
                None => {
                    // New flow: one back socket towards the target per client address.
                    let Ok(back) = UdpSocket::bind("127.0.0.1:0").await else {
                        continue;
                    };
                    if back.connect(target).await.is_err() {
                        continue;
                    }
                    let back = Arc::new(back);
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let b = back.clone();
                    let c2 = ctl.clone();
                    let up_tx = delay_line(ctl.clone(), seed, move |pkt| {
                        if b.try_send(&pkt).is_ok() {
                            c2.forwarded.fetch_add(1, Ordering::Relaxed);
                        }
                    });
                    let f = front.clone();
                    let c3 = ctl.clone();
                    let down_tx = delay_line(ctl.clone(), seed ^ 0xdead_beef, move |pkt| {
                        if f.try_send_to(&pkt, client).is_ok() {
                            c3.forwarded.fetch_add(1, Ordering::Relaxed);
                        }
                    });
                    let reader = tokio::spawn(async move {
                        let mut buf = vec![0u8; 65536];
                        while let Ok(n) = back.recv(&mut buf).await {
                            if down_tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                    });
                    task_list.lock().unwrap().push(reader);
                    flows.insert(client, up_tx.clone());
                    up_tx
                }
            };
            let _ = up.send(buf[..n].to_vec());
        }
    });
    tasks.lock().unwrap().push(main);
    Ok(UdpShaper {
        addr,
        control,
        tasks,
    })
}

/// Replace the `/ip4/../udp/<port>` part of a share dial address with the shaper's.
pub fn via(dial: &libp2p::Multiaddr, shaper: SocketAddr) -> libp2p::Multiaddr {
    use libp2p::multiaddr::Protocol;
    dial.iter()
        .map(|p| match p {
            Protocol::Udp(_) => Protocol::Udp(shaper.port()),
            other => other,
        })
        .collect()
}

/// UDP port of a share listen address.
pub fn udp_target(listen: &libp2p::Multiaddr) -> SocketAddr {
    use libp2p::multiaddr::Protocol;
    let mut ip = None;
    let mut port = None;
    for p in listen.iter() {
        match p {
            Protocol::Ip4(a) => ip = Some(std::net::IpAddr::V4(a)),
            Protocol::Udp(u) => port = Some(u),
            _ => {}
        }
    }
    SocketAddr::new(ip.expect("ip4"), port.expect("udp"))
}
