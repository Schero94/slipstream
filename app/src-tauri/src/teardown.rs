//! Owned-PID server teardown for Metal (llama-server) and MLX (run_omlx_pgrn / omlx).
//!
//! Policy (CRASH_AVOIDANCE):
//! - SIGTERM the child Slipstream started first (launcher EXIT traps release the lock).
//! - Never broad `pkill -f omlx-server`.
//! - Fallback: kill the lockfile holder when port matches, then port-scoped llama-server,
//!   then the single listener PID on our port via `lsof`.
//! - Clean `/tmp/slipstream-omlx-pgrn.lock` when the holder is dead or we stopped it.
//! - Clear `/tmp/slipstream-HANDS_OFF_OMLX.txt` after a product Stop.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

pub const DEFAULT_OMLX_LOCK: &str = "/tmp/slipstream-omlx-pgrn.lock";
pub const DEFAULT_HANDS_OFF: &str = "/tmp/slipstream-HANDS_OFF_OMLX.txt";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockMeta {
    pub pid: u32,
    pub port: Option<u16>,
}

/// Parse `pid=` / `port=` lines from a serve lockfile body.
pub fn parse_lock_body(body: &str) -> Option<LockMeta> {
    let mut pid: Option<u32> = None;
    let mut port: Option<u16> = None;
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("pid=") {
            if let Ok(v) = rest.trim().parse::<u32>() {
                if v > 0 {
                    pid = Some(v);
                }
            }
        } else if let Some(rest) = line.strip_prefix("port=") {
            let t = rest.trim();
            if t != "unknown" {
                if let Ok(v) = t.parse::<u16>() {
                    port = Some(v);
                }
            }
        }
    }
    pid.map(|pid| LockMeta { pid, port })
}

pub fn read_lock_file(path: &Path) -> Option<LockMeta> {
    let body = fs::read_to_string(path).ok()?;
    parse_lock_body(&body)
}

pub fn pid_alive(pid: u32) -> bool {
    // kill -0 is true for zombies too; treat state Z as gone so Stop doesn't hang.
    let ps = Command::new("ps")
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
            if st.starts_with('Z') || st.contains("Z") {
                return false;
            }
            return true;
        }
        return false;
    }
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// SIGTERM, wait up to `wait_ms`, then SIGKILL. Returns true if the process is gone.
pub fn terminate_pid(pid: u32, wait_ms: u64) -> bool {
    if !pid_alive(pid) {
        return true;
    }
    let _ = Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .status();
    let steps = (wait_ms / 100).max(1);
    for _ in 0..steps {
        if !pid_alive(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    if pid_alive(pid) {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", &pid.to_string()])
            .status();
        thread::sleep(Duration::from_millis(200));
    }
    !pid_alive(pid)
}

/// Graceful stop for a Child we own (SIGTERM → wait → SIGKILL).
pub fn kill_child_graceful(slot: &Mutex<Option<Child>>, wait_ms: u64) -> Option<u32> {
    let mut g = slot.lock().ok()?;
    let mut c = g.take()?;
    let pid = c.id();
    let _ = Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .status();
    let steps = (wait_ms / 100).max(1);
    for _ in 0..steps {
        if matches!(c.try_wait(), Ok(Some(_))) {
            return Some(pid);
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = c.kill();
    let _ = c.wait();
    Some(pid)
}

/// Remove lockfile when missing, holder dead, or holder matches `owner_pid`.
pub fn release_lock_if_safe(lock_path: &Path, owner_pid: Option<u32>) -> bool {
    if !lock_path.exists() {
        return true;
    }
    let Some(meta) = read_lock_file(lock_path) else {
        let _ = fs::remove_file(lock_path);
        return true;
    };
    if !pid_alive(meta.pid) {
        let _ = fs::remove_file(lock_path);
        return true;
    }
    if let Some(own) = owner_pid {
        if own == meta.pid {
            let _ = fs::remove_file(lock_path);
            return true;
        }
    }
    false
}

pub fn clear_hands_off(path: &Path) {
    let _ = fs::remove_file(path);
}

/// PIDs listening on 127.0.0.1:`port` (IPv4). Empty if none / lsof missing.
pub fn listener_pids_on_port(port: u16) -> Vec<u32> {
    let out = Command::new("lsof")
        .args([
            "-nP",
            &format!("-iTCP:127.0.0.1:{port}"),
            "-sTCP:LISTEN",
            "-t",
        ])
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|s| s.parse::<u32>().ok())
        .filter(|&p| p > 0)
        .collect()
}

/// Port-scoped Metal fallback only (not a broad omlx-server pkill).
pub fn pkill_llama_on_port(port: u16) {
    let _ = Command::new("pkill")
        .args(["-f", &format!("llama-server.*--port {port}")])
        .status();
}

/// Full Stop path used by the UI / tray.
pub fn stop_server(
    slot: &Mutex<Option<Child>>,
    server_port: u16,
    port_still_alive: impl Fn(u16) -> bool,
    lock_path: &Path,
    hands_off_path: &Path,
) {
    // 1) Owned child first — bash launcher traps EXIT and kills Python + drops lock.
    let owned = kill_child_graceful(slot, 2500);

    // 2) Lockfile holder (Python child / orphaned omlx) when it owns our port.
    if let Some(meta) = read_lock_file(lock_path) {
        let port_ok = meta.port.map(|p| p == server_port).unwrap_or(true);
        if port_ok && pid_alive(meta.pid) {
            // Skip if we already TERMed this exact pid as the owned child.
            if owned != Some(meta.pid) {
                let _ = terminate_pid(meta.pid, 2500);
            }
        }
        let _ = release_lock_if_safe(lock_path, Some(meta.pid));
    } else {
        let _ = release_lock_if_safe(lock_path, owned);
    }

    // 3) Metal leftover from a previous app instance — port-scoped only.
    if port_still_alive(server_port) {
        pkill_llama_on_port(server_port);
        thread::sleep(Duration::from_millis(200));
    }

    // 4) Last resort: the single listener(s) on our bound port (no name match).
    if port_still_alive(server_port) {
        for pid in listener_pids_on_port(server_port) {
            if owned == Some(pid) {
                continue;
            }
            let _ = terminate_pid(pid, 1500);
        }
    }

    let _ = release_lock_if_safe(lock_path, None);
    clear_hands_off(hands_off_path);
}

pub fn default_lock_path() -> PathBuf {
    PathBuf::from(
        std::env::var("SLIPSTREAM_PGRN_LOCK").unwrap_or_else(|_| DEFAULT_OMLX_LOCK.into()),
    )
}

pub fn default_hands_off_path() -> PathBuf {
    PathBuf::from(DEFAULT_HANDS_OFF)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::Stdio;

    #[test]
    fn parse_lock_reads_pid_and_port() {
        let m = parse_lock_body("pid=12345\nport=8080\nstarted_at=x\n").unwrap();
        assert_eq!(m.pid, 12345);
        assert_eq!(m.port, Some(8080));
    }

    #[test]
    fn parse_lock_unknown_port() {
        let m = parse_lock_body("pid=9\nport=unknown\n").unwrap();
        assert_eq!(m.pid, 9);
        assert_eq!(m.port, None);
    }

    #[test]
    fn parse_lock_rejects_bad_pid() {
        assert!(parse_lock_body("pid=0\nport=8080\n").is_none());
        assert!(parse_lock_body("port=8080\n").is_none());
    }

    #[test]
    fn release_lock_removes_dead_holder() {
        let dir = std::env::temp_dir().join(format!("ss-lock-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let lock = dir.join("lock");
        {
            let mut f = fs::File::create(&lock).unwrap();
            // PID 1 is always "alive" on Unix — use a high unused pid instead.
            writeln!(f, "pid=999999\nport=8080").unwrap();
        }
        // 999999 is almost certainly dead
        assert!(release_lock_if_safe(&lock, None));
        assert!(!lock.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn terminate_sleep_child() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        assert!(terminate_pid(pid, 2000), "SIGTERM/KILL should stop sleep");
        let _ = child.wait(); // reap so the pid cannot linger as zombie
        assert!(!pid_alive(pid));
    }
}
