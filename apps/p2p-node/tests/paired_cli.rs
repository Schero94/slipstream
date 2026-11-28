//! v0.6 M1 headless CLI: `slipstream-node share` / `use` / `peer-id` as separate
//! processes with their own data dirs (two Macs on one machine), against the mock
//! llama-server.

use std::io::{Read, Write};
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

#[test]
fn share_and_use_processes_forward_allowlisted_requests() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt
        .block_on(p2p_forward::mock_llama::start("127.0.0.1:0".parse().unwrap()))
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let big = dir.path().join("big-mac");
    let book = dir.path().join("macbook");
    let out = Command::new(bin()).args(["peer-id", "--data-dir"]).arg(&book).output().unwrap();
    assert!(out.status.success());
    let book_id = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert!(book_id.starts_with("12D3KooW"), "{book_id}");
    // stable identity per data dir
    let again = Command::new(bin()).args(["peer-id", "--data-dir"]).arg(&book).output().unwrap();
    assert_eq!(String::from_utf8(again.stdout).unwrap().trim(), book_id);

    let addr_file = dir.path().join("addr");
    let _share = Kill(
        Command::new(bin())
            .args(["share", "--listen", "/ip4/127.0.0.1/udp/0/quic-v1", "--forward"])
            .arg(format!("http://{}", mock.addr))
            .arg("--data-dir")
            .arg(&big)
            .args(["--allow-peer", &book_id, "--name", "CLI Studio", "--addr-file"])
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
    let dial = std::fs::read_to_string(&addr_file).unwrap().trim().to_string();
    assert!(dial.contains("/p2p/12D3KooW"), "{dial}");
    let port = free_port();
    let local: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let _use = Kill(
        Command::new(bin())
            .args(["use", "--peer", &dial, "--http", &local.to_string(), "--data-dir"])
            .arg(&book)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(local).is_err() {
        assert!(Instant::now() < deadline, "use never listened");
        std::thread::sleep(Duration::from_millis(50));
    }
    let models = http(local, format!("GET /v1/models HTTP/1.1\r\nHost: {local}\r\nConnection: close\r\n\r\n"));
    assert!(models.starts_with("HTTP/1.1 200"), "{models}");
    assert!(models.contains("\"slipstream\""));
    let body = r#"{"stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
    let chat = http(
        local,
        format!(
            "POST /v1/chat/completions?n=5 HTTP/1.1\r\nHost: {local}\r\nAuthorization: Bearer x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    );
    assert!(chat.contains("data: [DONE]"), "{chat}");
    let save = http(
        local,
        format!("POST /slots/0?action=save HTTP/1.1\r\nHost: {local}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"),
    );
    assert!(save.starts_with("HTTP/1.1 403"), "{save}");
    assert!(save.contains("Not available through a shared Mac"));
    let seen = mock.requests();
    assert!(seen.iter().all(|s| !s.target.starts_with("/slots/")));
    assert!(seen.iter().all(|s| s.headers.iter().all(|(k, _)| k != "authorization")));
}

#[test]
fn share_requires_allow_peer_and_job_api_refuses_paired_mode() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["share", "--data-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--allow-peer"));
    let out = Command::new(bin())
        .args(["share", "--forward", "http://192.168.1.5:8080", "--allow-peer"])
        .arg("12D3KooWKiMNN7kPTPiEWsKFk52qmRcLjE1W6KFZiyMdeu3AB9S9")
        .arg("--data-dir")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("loopback"));
    let out = Command::new(bin())
        .args(["mesh-serve", "--mode", "paired", "--key"])
        .arg(dir.path().join("k"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("paired"));
    let out = Command::new(bin())
        .args(["use", "--peer", "/ip4/127.0.0.1/udp/9/quic-v1", "--data-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("pinned"));
}
