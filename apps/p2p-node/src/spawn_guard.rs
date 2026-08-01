//! Pre-flight guards for CLI `--spawn-engine` (no dual heavy serve).
//!
//! Refuses when:
//! 1. `/tmp/slipstream-omlx-pgrn.lock` (or `SLIPSTREAM_PGRN_LOCK`) holds a live pid, or
//! 2. the configured infer endpoint already answers as Slipstream / llama / oMLX.
//!
//! These checks never start an engine; they only read the lockfile and probe HTTP.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use p2p_engine::EngineChoice;

/// Default singleton lock written by `run_omlx_pgrn.sh` / product Start.
pub const DEFAULT_OMLX_LOCK: &str = "/tmp/slipstream-omlx-pgrn.lock";

/// Prefix shared by refuse strings (stable for CLI + tests).
pub const REFUSE_PREFIX: &str = "REFUSE: --spawn-engine blocked";

const PROBE_TIMEOUT: Duration = Duration::from_millis(400);

/// Lock path: `SLIPSTREAM_PGRN_LOCK` or [`DEFAULT_OMLX_LOCK`].
pub fn default_lock_path() -> PathBuf {
    PathBuf::from(
        std::env::var("SLIPSTREAM_PGRN_LOCK").unwrap_or_else(|_| DEFAULT_OMLX_LOCK.into()),
    )
}

/// Infer endpoint that `--spawn-engine` would fight over (env / engine defaults).
pub fn resolve_guard_endpoint(choice: EngineChoice, os: &str) -> String {
    match choice.to_backend(os) {
        Some(p2p_core::BackendKind::LlamaPgrn) => llama_endpoint(),
        _ => mlx_endpoint(),
    }
}

fn mlx_endpoint() -> String {
    if let Ok(ep) = std::env::var("P2P_MLX_ENDPOINT") {
        if !ep.trim().is_empty() {
            return normalize_endpoint(&ep);
        }
    }
    if let Ok(ep) = std::env::var("SLIPSTREAM_MLX_ENDPOINT") {
        if !ep.trim().is_empty() {
            return normalize_endpoint(&ep);
        }
    }
    let port = std::env::var("P2P_MLX_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    format!("http://127.0.0.1:{port}")
}

fn llama_endpoint() -> String {
    if let Ok(ep) = std::env::var("P2P_LLAMA_ENDPOINT") {
        if !ep.trim().is_empty() {
            return normalize_endpoint(&ep);
        }
    }
    if let Ok(ep) = std::env::var("SLIPSTREAM_LLAMA_ENDPOINT") {
        if !ep.trim().is_empty() {
            return normalize_endpoint(&ep);
        }
    }
    let host = std::env::var("P2P_LLAMA_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port = std::env::var("P2P_LLAMA_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8081);
    format!("http://{host}:{port}")
}

pub fn normalize_endpoint(base: &str) -> String {
    let mut s = base.trim().to_string();
    while s.ends_with('/') {
        s.pop();
    }
    s
}

/// Parse `pid=` from a lockfile body (`key=value` lines).
pub fn parse_lock_pid(body: &str) -> Option<u32> {
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("pid=") {
            if let Ok(v) = rest.trim().parse::<u32>() {
                if v > 0 {
                    return Some(v);
                }
            }
        }
    }
    None
}

pub fn read_lock_pid(path: &Path) -> Option<u32> {
    let body = std::fs::read_to_string(path).ok()?;
    parse_lock_pid(&body)
}

/// True if `pid` is alive (and not a zombie). Best-effort; false on probe failure.
pub fn pid_alive(pid: u32) -> bool {
    let ps = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "state="])
        .output()
        .ok();
    if let Some(out) = ps {
        if out.status.success() {
            let st = String::from_utf8_lossy(&out.stdout);
            let st = st.trim();
            if st.is_empty() {
                return false;
            }
            if st.starts_with('Z') || st.contains('Z') {
                return false;
            }
            return true;
        }
        return false;
    }
    std::process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// True when `{endpoint}/health` or `{endpoint}/v1/models` returns any HTTP response.
///
/// Matches product readiness probes: llama/oMLX may answer `/health` with 503 while
/// loading — still means the port is owned (dual-serve risk).
pub fn endpoint_looks_healthy(endpoint: &str) -> bool {
    let base = normalize_endpoint(endpoint);
    http_responds(&base, "/health") || http_responds(&base, "/v1/models")
}

fn http_responds(endpoint: &str, path: &str) -> bool {
    let Some((host, port)) = parse_http_host_port(endpoint) else {
        return false;
    };
    let addr = match (host.as_str(), port).to_socket_addrs() {
        Ok(mut it) => match it.next() {
            Some(a) => a,
            None => return false,
        },
        Err(_) => return false,
    };
    probe_http_get(addr, &host, path).is_some()
}

fn parse_http_host_port(endpoint: &str) -> Option<(String, u16)> {
    let rest = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    if let Some((h, p)) = authority.rsplit_once(':') {
        let port: u16 = p.parse().ok()?;
        let host = h.trim_start_matches('[').trim_end_matches(']').to_string();
        if host.is_empty() {
            return None;
        }
        Some((host, port))
    } else {
        let default_port = if endpoint.starts_with("https://") {
            443
        } else {
            80
        };
        Some((authority.to_string(), default_port))
    }
}

fn probe_http_get(addr: SocketAddr, host: &str, path: &str) -> Option<u16> {
    let mut stream = TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).ok()?;
    stream.set_read_timeout(Some(PROBE_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(PROBE_TIMEOUT)).ok()?;
    let req = format!(
        "GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = [0u8; 96];
    let n = stream.read(&mut buf).ok()?;
    if n == 0 {
        return None;
    }
    let head = std::str::from_utf8(&buf[..n]).ok()?;
    if !head.starts_with("HTTP/") {
        return None;
    }
    let code = head.split_whitespace().nth(1)?.parse().ok()?;
    Some(code)
}

/// Refuse message when the serve lock is held by a live process.
pub fn refuse_live_lock_msg(lock_path: &Path, pid: u32) -> String {
    format!(
        "{REFUSE_PREFIX}: live serve lock at {} (pid={pid}). \
         Dual heavy serve risks a Mac freeze — tear down that holder or attach via HTTP without --spawn-engine.",
        lock_path.display()
    )
}

/// Refuse message when the infer endpoint already looks like a running serve.
pub fn refuse_healthy_endpoint_msg(endpoint: &str) -> String {
    format!(
        "{REFUSE_PREFIX}: healthy Slipstream/llama/omlx serve already at {endpoint}. \
         Dual-serve freeze risk — omit --spawn-engine and use the existing serve, or stop it first."
    )
}

/// Core guard with injectable probes (unit tests; no real engines).
pub fn check_spawn_engine_safe_with(
    lock_path: &Path,
    endpoint: &str,
    pid_alive_fn: impl Fn(u32) -> bool,
    endpoint_healthy_fn: impl Fn(&str) -> bool,
) -> Result<(), String> {
    if lock_path.is_file() {
        if let Some(pid) = read_lock_pid(lock_path) {
            if pid_alive_fn(pid) {
                return Err(refuse_live_lock_msg(lock_path, pid));
            }
        }
    }
    let endpoint = normalize_endpoint(endpoint);
    if endpoint_healthy_fn(&endpoint) {
        return Err(refuse_healthy_endpoint_msg(&endpoint));
    }
    Ok(())
}

/// Production guard: live lock pid + HTTP `/health` or `/v1/models` probe.
pub fn check_spawn_engine_safe(lock_path: &Path, endpoint: &str) -> Result<(), String> {
    check_spawn_engine_safe_with(lock_path, endpoint, pid_alive, endpoint_looks_healthy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parse_lock_pid_reads_key_value() {
        assert_eq!(
            parse_lock_pid("pid=4242\nport=8080\nstarted_at=x\n"),
            Some(4242)
        );
        assert_eq!(parse_lock_pid("port=8080\n"), None);
        assert_eq!(parse_lock_pid("pid=0\n"), None);
    }

    #[test]
    fn refuses_when_lock_pid_alive() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("slipstream-omlx-pgrn.lock");
        {
            let mut f = std::fs::File::create(&lock).unwrap();
            writeln!(f, "pid={}", std::process::id()).unwrap();
            writeln!(f, "port=8080").unwrap();
        }
        let err = check_spawn_engine_safe_with(
            &lock,
            "http://127.0.0.1:18080",
            |_| true,
            |_| false,
        )
        .unwrap_err();
        assert!(err.contains(REFUSE_PREFIX), "{err}");
        assert!(err.contains("live serve lock"), "{err}");
        assert!(err.contains(&std::process::id().to_string()), "{err}");
        assert!(err.contains(lock.file_name().unwrap().to_str().unwrap()), "{err}");
    }

    #[test]
    fn allows_when_lock_pid_dead() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("lock");
        {
            let mut f = std::fs::File::create(&lock).unwrap();
            writeln!(f, "pid=999999").unwrap();
            writeln!(f, "port=8080").unwrap();
        }
        check_spawn_engine_safe_with(
            &lock,
            "http://127.0.0.1:18080",
            |_| false, // dead
            |_| false,
        )
        .expect("stale lock must not refuse");
    }

    #[test]
    fn refuses_when_endpoint_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("missing.lock");
        let err = check_spawn_engine_safe_with(
            &lock,
            "http://127.0.0.1:8080",
            |_| false,
            |_| true,
        )
        .unwrap_err();
        assert!(err.contains(REFUSE_PREFIX), "{err}");
        assert!(err.contains("healthy Slipstream/llama/omlx serve"), "{err}");
        assert!(err.contains("http://127.0.0.1:8080"), "{err}");
        assert!(err.contains("Dual-serve"), "{err}");
    }

    #[test]
    fn allows_when_no_lock_and_endpoint_down() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("nope.lock");
        check_spawn_engine_safe_with(
            &lock,
            "http://127.0.0.1:18080",
            |_| true,
            |_| false,
        )
        .expect("clear path");
    }

    #[test]
    fn refuse_messages_are_stable() {
        let lock = Path::new("/tmp/slipstream-omlx-pgrn.lock");
        let m = refuse_live_lock_msg(lock, 42);
        assert_eq!(
            m,
            "REFUSE: --spawn-engine blocked: live serve lock at /tmp/slipstream-omlx-pgrn.lock (pid=42). \
             Dual heavy serve risks a Mac freeze — tear down that holder or attach via HTTP without --spawn-engine."
        );
        let m2 = refuse_healthy_endpoint_msg("http://127.0.0.1:8080");
        assert_eq!(
            m2,
            "REFUSE: --spawn-engine blocked: healthy Slipstream/llama/omlx serve already at http://127.0.0.1:8080. \
             Dual-serve freeze risk — omit --spawn-engine and use the existing serve, or stop it first."
        );
    }

    #[test]
    fn live_pid_probe_sees_self() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(999_999));
    }
}
