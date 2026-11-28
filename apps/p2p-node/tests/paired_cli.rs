//! v0.6 headless CLI: `slipstream-node pair` / `share` / `use` / `trust` / `peer-id` /
//! `reset-identity` as separate processes with their own data dirs (two Macs on one
//! machine), against the mock llama-server. M2: peers come from the trust store.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_slipstream-node")
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn http(addr: SocketAddr, req: String) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

fn run(args: &[&str], dir: &std::path::Path) -> std::process::Output {
    Command::new(bin()).args(args).arg("--data-dir").arg(dir).output().unwrap()
}

fn text(o: &std::process::Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

/// `pair --show-code` on the big Mac, `pair --peer --code` on the MacBook. Returns the
/// UDP port the big Mac paired on.
fn pair_via_cli(big: &std::path::Path, book: &std::path::Path) -> (String, std::time::Duration) {
    let mut show = Command::new(bin())
        .args(["pair", "--show-code", "--listen", "/ip4/127.0.0.1/udp/0/quic-v1", "--name", "CLI Studio", "--data-dir"])
        .arg(big)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(show.stdout.take().unwrap()).lines();
    let (mut code, mut dial) = (None, None);
    while code.is_none() || dial.is_none() {
        let line = lines.next().expect("pair --show-code exited early").unwrap();
        if let Some(c) = line.strip_prefix("code=") {
            code = Some(c.to_string());
        }
        if let Some(d) = line.strip_prefix("dial=") {
            dial = Some(d.to_string());
        }
    }
    let (code, dial) = (code.unwrap(), dial.unwrap());
    assert_eq!(code.len(), 6);
    let t0 = Instant::now();
    let out = Command::new(bin())
        .args(["pair", "--peer", &dial, "--code", &code, "--name", "MacBook", "--data-dir"])
        .arg(book)
        .output()
        .unwrap();
    let took = t0.elapsed();
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("paired with \"CLI Studio\""), "{}", text(&out));
    let status = show.wait().unwrap();
    assert!(status.success(), "pair --show-code did not finish cleanly");
    let rest: Vec<String> = lines.map_while(Result::ok).collect();
    assert!(rest.iter().any(|l| l.contains("paired with \"MacBook\"")), "{rest:?}");
    let port = dial.split("/udp/").nth(1).unwrap().split('/').next().unwrap().to_string();
    (port, took)
}

#[test]
fn pair_share_use_trust_revoke_end_to_end() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt
        .block_on(p2p_forward::mock_llama::start("127.0.0.1:0".parse().unwrap()))
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let big = dir.path().join("big-mac");
    let book = dir.path().join("macbook");
    let out = run(&["peer-id"], &book);
    assert!(out.status.success());
    let book_id = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert!(book_id.starts_with("12D3KooW"), "{book_id}");
    assert_eq!(String::from_utf8(run(&["peer-id"], &book).stdout).unwrap().trim(), book_id);

    let (port, took) = pair_via_cli(&big, &book);
    println!("cli_pairing_ms={:.1} (client process incl. startup)", took.as_secs_f64() * 1e3);
    let list = text(&run(&["trust", "list"], &book));
    assert!(list.contains("CLI Studio") && list.contains("shares with this Mac"), "{list}");
    let list = text(&run(&["trust", "list"], &big));
    assert!(list.contains("MacBook") && list.contains(&book_id) && list.contains("uses this Mac"), "{list}");

    // share on the port it paired on (the MacBook stored that address)
    let addr_file = dir.path().join("addr");
    let _share = Kill(
        Command::new(bin())
            .args(["share", "--listen", &format!("/ip4/127.0.0.1/udp/{port}/quic-v1"), "--forward"])
            .arg(format!("http://{}", mock.addr))
            .arg("--data-dir")
            .arg(&big)
            .args(["--name", "CLI Studio", "--addr-file"])
            .arg(&addr_file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::read_to_string(&addr_file).map(|s| s.trim().is_empty()).unwrap_or(true) {
        assert!(Instant::now() < deadline, "share never wrote its address");
        std::thread::sleep(Duration::from_millis(50));
    }
    let port_http = free_port();
    let local: SocketAddr = format!("127.0.0.1:{port_http}").parse().unwrap();
    // no --peer: the only Mac that shares with this one, from the trust store
    let mut use_proc = Kill(
        Command::new(bin())
            .args(["use", "--http", &local.to_string(), "--data-dir"])
            .arg(&book)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(local).is_err() {
        assert!(Instant::now() < deadline, "use never listened");
        std::thread::sleep(Duration::from_millis(50));
    }
    // M5: the editor port is bound at once; until the paired connection is up it answers
    // 503 (peer_waking), so wait for the first 200.
    // Review M2: `use` requires the local token by default (<data dir>/local.token, 0600).
    let token = std::fs::read_to_string(book.join("local.token")).unwrap().trim().to_string();
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(book.join("local.token")).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    let models = loop {
        let m = http(local, format!("GET /v1/models HTTP/1.1\r\nHost: {local}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"));
        if m.starts_with("HTTP/1.1 200") || Instant::now() > deadline {
            break m;
        }
        assert!(m.starts_with("HTTP/1.1 503") && m.contains("peer_waking"), "{m}");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(models.starts_with("HTTP/1.1 200"), "{models}");
    assert!(models.contains("\"slipstream\""));
    let body = r#"{"stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
    let chat = http(
        local,
        format!(
            "POST /v1/chat/completions?n=5 HTTP/1.1\r\nHost: {local}\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    );
    assert!(chat.contains("data: [DONE]"), "{chat}");
    let anon = http(local, format!("GET /v1/models HTTP/1.1\r\nHost: {local}\r\nConnection: close\r\n\r\n"));
    assert!(anon.starts_with("HTTP/1.1 401"), "no token must be refused: {anon}");
    let save = http(
        local,
        format!("POST /slots/0?action=save HTTP/1.1\r\nHost: {local}\r\nAuthorization: Bearer {token}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"),
    );
    assert!(save.starts_with("HTTP/1.1 403"), "{save}");
    assert!(save.contains("Not available through a shared Mac"));
    let seen = mock.requests();
    assert!(seen.iter().all(|s| !s.target.starts_with("/slots/")));
    assert!(seen.iter().all(|s| s.headers.iter().all(|(k, _)| k != "authorization")));

    // `trust revoke` on the big Mac while share runs: the share process drops the
    // MacBook (file watcher), sends Unpair; the use process removes the big Mac.
    let out = run(&["trust", "revoke", "MacBook"], &big);
    assert!(out.status.success(), "{}", text(&out));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let l = text(&run(&["trust", "list"], &book));
        if l.contains("no paired Macs") {
            break;
        }
        assert!(Instant::now() < deadline, "MacBook never got the Unpair: {l}");
        std::thread::sleep(Duration::from_millis(100));
    }
    // M5: the use process (same session code as the app) ends with "removed this Mac"
    // and releases the editor port; nothing more reaches llama-server.
    let n = mock.requests().len();
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(st) = use_proc.0.try_wait().unwrap() {
            break st;
        }
        assert!(Instant::now() < deadline, "use did not stop after being removed");
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut err = String::new();
    use_proc.0.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    assert!(!status.success());
    assert!(err.contains("removed this Mac"), "{err}");
    assert!(TcpStream::connect(local).is_err(), "editor port released");
    assert_eq!(mock.requests().len(), n);
}

#[test]
fn share_and_use_need_pairing_and_reset_identity_needs_confirm() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(&["share"], dir.path());
    assert!(!out.status.success());
    assert!(text(&out).contains("No paired Macs"), "{}", text(&out));
    let out = run(&["share", "--forward", "http://192.168.1.5:8080"], dir.path());
    assert!(!out.status.success());
    assert!(text(&out).contains("loopback"));
    let out = Command::new(bin())
        .args(["mesh-serve", "--mode", "paired", "--key"])
        .arg(dir.path().join("k"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(text(&out).contains("paired"));
    let out = run(&["use"], dir.path());
    assert!(!out.status.success());
    assert!(text(&out).contains("No paired Mac"), "{}", text(&out));
    let out = run(
        &["use", "--peer", "/ip4/127.0.0.1/udp/9/quic-v1/p2p/12D3KooWKiMNN7kPTPiEWsKFk52qmRcLjE1W6KFZiyMdeu3AB9S9"],
        dir.path(),
    );
    assert!(!out.status.success());
    assert!(text(&out).contains("not a paired Mac"), "{}", text(&out));
    let out = run(&["use", "--peer", "/ip4/127.0.0.1/udp/9/quic-v1"], dir.path());
    assert!(!out.status.success());
    assert!(text(&out).contains("pinned"), "{}", text(&out));
    let out = run(&["pair", "--peer", "/ip4/127.0.0.1/udp/9/quic-v1", "--code", "12345"], dir.path());
    assert!(!out.status.success());
    assert!(text(&out).contains("6 digits"), "{}", text(&out));
    let out = run(&["trust", "revoke", "nobody"], dir.path());
    assert!(!out.status.success());

    let before = String::from_utf8(run(&["peer-id"], dir.path()).stdout).unwrap();
    let out = run(&["reset-identity"], dir.path());
    assert!(!out.status.success());
    assert!(text(&out).contains("--confirm"), "{}", text(&out));
    assert_eq!(String::from_utf8(run(&["peer-id"], dir.path()).stdout).unwrap(), before);
    let out = run(&["reset-identity", "--confirm"], dir.path());
    assert!(out.status.success(), "{}", text(&out));
    let after = String::from_utf8(run(&["peer-id"], dir.path()).stdout).unwrap();
    assert_ne!(after, before);
    assert!(text(&run(&["trust", "list"], dir.path())).contains("no paired Macs"));
}
