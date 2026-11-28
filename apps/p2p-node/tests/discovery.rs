//! v0.6 M3: LAN discovery end to end with separate `slipstream-node` processes on one
//! host (plan §7 / §9 M3): `share` announces `_slipstream._udp`, `discover` lists it,
//! `use` finds the pinned Mac by fingerprint, re-dials after the share restarts on a new
//! port, `pair --show-code` opens the window inside the running share (pair flag), and
//! `pair --peer <fp>` pairs a third Mac without an address. Spoof: an announcement of
//! the paired Mac's fingerprint at an impostor's address is never dialed as that Mac.
//!
//! Needs multicast on a LAN interface (mdns-sd sends on non-loopback interfaces). Skips
//! with a clear message when unavailable or when `SLIPSTREAM_NO_MULTICAST` is set.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Proc {
    child: Child,
    lines: Arc<Mutex<Vec<(Instant, String)>>>,
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Proc {
    /// Spawn with stdout+stderr captured line by line (with arrival time).
    fn spawn(args: &[&str], dir: &Path) -> Proc {
        let mut child = Command::new(bin())
            .args(args)
            .arg("--data-dir")
            .arg(dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        for r in [
            Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
            Box::new(child.stderr.take().unwrap()),
        ] {
            let lines = lines.clone();
            std::thread::spawn(move || {
                for l in BufReader::new(r).lines().map_while(Result::ok) {
                    lines.lock().unwrap().push((Instant::now(), l));
                }
            });
        }
        Proc { child, lines }
    }

    fn wait_line(&self, within: Duration, pred: impl Fn(&str) -> bool) -> Option<(Instant, String)> {
        let t0 = Instant::now();
        while t0.elapsed() < within {
            if let Some(hit) = self.lines.lock().unwrap().iter().find(|(_, l)| pred(l)).cloned() {
                return Some(hit);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    fn dump(&self) -> String {
        self.lines.lock().unwrap().iter().map(|(_, l)| l.clone()).collect::<Vec<_>>().join("\n")
    }

    /// SIGTERM (graceful: mDNS goodbye, control socket removed) and wait; returns the log.
    fn term(mut self) -> String {
        let _ = Command::new("kill").arg("-TERM").arg(self.child.id().to_string()).status();
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(5) {
            if let Some(st) = self.child.try_wait().unwrap() {
                std::thread::sleep(Duration::from_millis(50));
                return format!("{}\nexit={st:?}", self.dump());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("process did not exit on SIGTERM");
    }
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_slipstream-node")
}

fn skip_reason() -> Option<String> {
    if std::env::var_os("SLIPSTREAM_NO_MULTICAST").is_some() {
        return Some("SLIPSTREAM_NO_MULTICAST is set".into());
    }
    if !p2p_net::discovery::mdns::lan_ipv4_available() {
        return Some("no non-loopback IPv4 interface (mDNS is sent on LAN interfaces only)".into());
    }
    if let Err(e) = p2p_net::discovery::mdns::browse_continuous() {
        return Some(format!("cannot open the mDNS socket: {e}"));
    }
    None
}

fn run(args: &[&str], dir: &Path) -> std::process::Output {
    Command::new(bin()).args(args).arg("--data-dir").arg(dir).output().unwrap()
}

fn text(o: &std::process::Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn get_models(addr: SocketAddr) -> String {
    let Ok(mut s) = TcpStream::connect(addr) else { return String::new() };
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let _ = s.write_all(format!("GET /v1/models HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes());
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

fn peer_id(dir: &Path) -> libp2p::PeerId {
    String::from_utf8(run(&["peer-id"], dir).stdout).unwrap().trim().parse().unwrap()
}

/// Pair over loopback with the address (pre-M3 path; nothing is announced on loopback).
fn pair_by_address(big: &Path, book: &Path, name: &str) {
    let show = Proc::spawn(&["pair", "--show-code", "--listen", "/ip4/127.0.0.1/udp/0/quic-v1", "--name", "Studio"], big);
    let code = show.wait_line(Duration::from_secs(10), |l| l.starts_with("code=")).expect("no code").1;
    let dial = show.wait_line(Duration::from_secs(10), |l| l.starts_with("dial=")).unwrap().1;
    let out = run(&["pair", "--peer", &dial[5..], "--code", &code[5..], "--name", name], book);
    assert!(out.status.success(), "{}", text(&out));
}

fn start_share(big: &Path, mock: SocketAddr) -> (Proc, u16) {
    let share = Proc::spawn(
        &["share", "--listen", "/ip4/0.0.0.0/udp/0/quic-v1", "--forward", &format!("http://{mock}"), "--name", "Studio"],
        big,
    );
    let line = share
        .wait_line(Duration::from_secs(10), |l| l.starts_with("sharing "))
        .unwrap_or_else(|| panic!("share did not start:\n{}", share.dump()))
        .1;
    assert!(line.contains("announced=true"), "{line}");
    let port: u16 = line.split("/udp/").nth(1).unwrap().split('/').next().unwrap().parse().unwrap();
    (share, port)
}

#[test]
fn discover_use_redial_pair_flag_and_pair_by_fingerprint() {
    if let Some(why) = skip_reason() {
        eprintln!("SKIP discovery: {why}");
        return;
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(p2p_forward::mock_llama::start("127.0.0.1:0".parse().unwrap())).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (big, book, third) = (dir.path().join("big"), dir.path().join("book"), dir.path().join("third"));
    pair_by_address(&big, &book, "MacBook");
    let big_id = peer_id(&big);
    let fp = p2p_node::lan::fingerprint(&big_id);

    // 1. share announces; `discover` on the MacBook lists it as paired within 3 s
    let (share, port1) = start_share(&big, mock.addr);
    let t0 = Instant::now();
    let out = run(&["discover", "--timeout-ms", "3000"], &book);
    let line = text(&out).lines().find(|l| l.starts_with(&fp)).map(str::to_string);
    let line = line.unwrap_or_else(|| panic!("share not discovered within 3 s:\n{}", text(&out)));
    println!("discover_ms={:.0} line={line}", t0.elapsed().as_secs_f64() * 1e3);
    assert!(line.contains("paired \"Studio\"") && line.contains(&format!(":{port1}")), "{line}");

    // 2. use finds it by fingerprint (its stored address is the old loopback pairing port)
    let http: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let t_use = Instant::now();
    let use_ = Proc::spawn(&["use", "--no-token", "--http", &http.to_string()], &book);
    use_.wait_line(Duration::from_secs(10), |l| l.starts_with("editors:"))
        .unwrap_or_else(|| panic!("use never connected:\n{}", use_.dump()));
    println!("use_connect_ms={:.0}", t_use.elapsed().as_secs_f64() * 1e3);
    let (t_found, _) = use_
        .wait_line(Duration::from_secs(3), |l| l.starts_with("lan event=found") && l.contains(&fp))
        .unwrap_or_else(|| panic!("use did not discover the share within 3 s:\n{}", use_.dump()));
    let found = t_found.duration_since(t_use);
    println!("use_found_ms={:.0} (process start → first Found)", found.as_secs_f64() * 1e3);
    assert!(found <= Duration::from_secs(3), "found after {found:?}");
    assert!(get_models(http).starts_with("HTTP/1.1 200"));
    // last_addrs updated to the working (discovered) address
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let l = text(&run(&["trust", "list"], &book));
        if l.contains(&format!("/udp/{port1}/quic-v1/p2p/{big_id}")) {
            break;
        }
        assert!(Instant::now() < deadline, "last_addrs not updated: {l}");
        std::thread::sleep(Duration::from_millis(100));
    }

    // 3. pairing window inside the running share: pair=1 appears, a third Mac pairs by
    //    fingerprint without an address, then the flag disappears
    let show = Proc::spawn(&["pair", "--show-code"], &big);
    let code = show
        .wait_line(Duration::from_secs(5), |l| l.starts_with("code="))
        .unwrap_or_else(|| panic!("no code from the running share:\n{}", show.dump()))
        .1[5..]
        .to_string();
    assert!(show.wait_line(Duration::from_secs(1), |l| l.contains("running share opened")).is_some());
    let t_flag = Instant::now();
    let flag_on = loop {
        let out = text(&run(&["discover", "--timeout-ms", "1500"], &third));
        if out.lines().any(|l| l.starts_with(&fp) && l.contains("\topen\t")) {
            break t_flag.elapsed();
        }
        assert!(t_flag.elapsed() < Duration::from_secs(6), "pair=1 never appeared: {out}");
    };
    println!("pair_flag_on_ms={:.0}", flag_on.as_secs_f64() * 1e3);
    let t_pair = Instant::now();
    let out = run(&["pair", "--peer", &fp[..6], "--code", &code, "--name", "Third"], &third);
    assert!(out.status.success(), "{}", text(&out));
    println!("pair_by_fp_ms={:.0}", t_pair.elapsed().as_secs_f64() * 1e3);
    assert!(text(&out).contains("paired with \"Studio\""), "{}", text(&out));
    assert!(show.wait_line(Duration::from_secs(3), |l| l.contains("paired with \"Third\"")).is_some(), "{}", show.dump());
    let t_off = Instant::now();
    loop {
        let out = text(&run(&["discover", "--timeout-ms", "1500"], &third));
        if out.lines().any(|l| l.starts_with(&fp) && l.contains("paired \"Studio\"") && l.contains("\t-\t")) {
            break;
        }
        assert!(t_off.elapsed() < Duration::from_secs(6), "pair=1 never disappeared: {out}");
    }
    println!("pair_flag_off_ms={:.0}", t_off.elapsed().as_secs_f64() * 1e3);

    // 4. share stops (graceful) → Lost; restarts on a new port → use re-dials ≤ 10 s
    let t_stop = Instant::now();
    let share_log = share.term();
    // Normally ~1.1 s (mDNS goodbye). If the goodbye is missed, the browser's periodic
    // re-verification drops it within REVERIFY_EVERY + REBROWSE_GRACE (13 s).
    use_.wait_line(Duration::from_secs(15), |l| l.starts_with("lan event=lost"))
        .unwrap_or_else(|| panic!("no Lost after the share stopped:\n{}\n--- share:\n{share_log}", use_.dump()));
    let lost = t_stop.elapsed();
    println!("lost_ms={:.0}{}", lost.as_secs_f64() * 1e3, if lost > Duration::from_secs(3) { " (goodbye missed; re-verify)" } else { "" });
    let n_conn = use_.lines.lock().unwrap().iter().filter(|(_, l)| l.starts_with("use event=connected")).count();
    let (_share2, port2) = start_share(&big, mock.addr);
    assert_ne!(port1, port2);
    let t_restart = Instant::now();
    let deadline = t_restart + Duration::from_secs(10);
    loop {
        let reconnected =
            use_.lines.lock().unwrap().iter().filter(|(_, l)| l.starts_with("use event=connected")).count() > n_conn;
        if reconnected && get_models(http).starts_with("HTTP/1.1 200") {
            break;
        }
        assert!(Instant::now() < deadline, "use did not re-dial within 10 s:\n{}", use_.dump());
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("redial_after_restart_ms={:.0}", t_restart.elapsed().as_secs_f64() * 1e3);
    let line = use_.dump().lines().rev().find(|l| l.starts_with("use event=connected")).unwrap().to_string();
    assert!(line.contains(&format!("/udp/{port2}/")), "{line}");
}

#[test]
fn spoofed_fingerprint_is_never_dialed_as_the_paired_mac() {
    if let Some(why) = skip_reason() {
        eprintln!("SKIP discovery: {why}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (big, book, evil) = (dir.path().join("big"), dir.path().join("book"), dir.path().join("evil"));
    pair_by_address(&big, &book, "MacBook");
    let big_id = peer_id(&big);
    let fp = p2p_node::lan::fingerprint(&big_id);
    // The real share is NOT running. An impostor (another identity) listens on the LAN
    // and someone announces the paired Mac's fingerprint at the impostor's port.
    let imp = Proc::spawn(&["pair", "--show-code", "--listen", "/ip4/0.0.0.0/udp/0/quic-v1", "--name", "Studio"], &evil);
    let dial = imp.wait_line(Duration::from_secs(10), |l| l.starts_with("dial=")).unwrap().1;
    let imp_port: u16 = dial.split("/udp/").nth(1).unwrap().split('/').next().unwrap().parse().unwrap();
    let _spoof = p2p_net::discovery::mdns::announce_share(&p2p_net::discovery::mdns::ShareAdvert {
        fp: fp.clone(),
        proto: p2p_net::discovery::mdns::ShareProto::QuicV1,
        pairing_open: false,
        port: imp_port,
    })
    .unwrap();
    let before = text(&run(&["trust", "list"], &book));
    let http: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let use_ = Proc::spawn(&["use", "--no-token", "--http", &http.to_string()], &book);
    // it must see the spoofed announcement, dial it as the pinned PeerId and fail
    use_.wait_line(Duration::from_secs(5), |l| l.starts_with("lan event=found") && l.contains(&fp))
        .unwrap_or_else(|| panic!("spoofed announcement not seen:\n{}", use_.dump()));
    let mut use_ = use_;
    let t0 = Instant::now();
    let status = loop {
        if let Some(s) = use_.child.try_wait().unwrap() {
            break s;
        }
        assert!(t0.elapsed() < Duration::from_secs(15), "use still running:\n{}", use_.dump());
        std::thread::sleep(Duration::from_millis(50));
    };
    let log = use_.dump();
    assert!(!status.success(), "use must not start against an impostor:\n{log}");
    assert!(!log.contains("use event=connected"), "{log}");
    assert!(!log.contains("editors:"), "{log}");
    assert!(log.to_lowercase().contains("peer id") || log.contains("could not reach"), "{log}");
    assert!(TcpStream::connect(http).is_err());
    // the impostor's address never became a "last address" of the paired Mac
    assert_eq!(text(&run(&["trust", "list"], &book)), before);
    assert!(!before.contains(&format!("/udp/{imp_port}/")));
    // and nobody paired with the impostor
    assert!(text(&run(&["trust", "list"], &evil)).contains("no paired Macs"));
}
