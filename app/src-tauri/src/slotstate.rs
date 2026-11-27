//! Keep the agent's prompt state across server restarts.
//!
//! A 40k-token coding-agent context takes ~2-3 minutes to read on a 36 GB Mac. llama-server keeps
//! it in RAM between turns, but every restart (settings change, app quit) threw it away. Before the
//! server stops we ask it to write slot 0 to disk (`/slots/0?action=save`, KV + recurrent state +
//! context checkpoints); after the next start, once the model is loaded, we restore it. The next
//! request then only reads its new tokens. A restored state is the same state the server had, so
//! answers are identical (bench E5).
//!
//! The file is keyed by everything that shapes the state (model, sidecar, context size, KV type), so
//! a different setup never loads a foreign state; llama-server also refuses a mismatching file.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// Never let saved prompt states eat the disk the engine needs (PGRN admission keeps ~16 GB free).
const MIN_FREE_AFTER_SAVE_GB: f64 = 24.0;
/// How many different setups keep a saved state (newest wins).
const MAX_FILES: usize = 2;

pub fn slot_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join("Library/Application Support/Slipstream/slots")
}

/// FNV-1a: stable across Rust versions (DefaultHasher is not), good enough for a file key.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// File name for a server setup. Model size is part of the key so a re-downloaded model with the
/// same path does not load a stale state.
pub fn slot_key(model: &str, pgrn: &str, ctx: u32, kv_quant: &str, model_bytes: u64) -> String {
    let id = format!("{model}|{pgrn}|{ctx}|{kv_quant}|{model_bytes}");
    format!("slipstream-{:016x}.bin", fnv1a(&id))
}

fn free_disk_gb(dir: &PathBuf) -> Option<f64> {
    let out = Command::new("df").args(["-k"]).arg(dir).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let avail_kb: f64 = s.lines().nth(1)?.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail_kb / 1_048_576.0)
}

fn post(port: u16, path: &str, body: &str, timeout_s: u32) -> Option<String> {
    let out = Command::new("curl")
        .args(["-s", "--fail", "-m", &timeout_s.to_string(), "-X", "POST",
               "-H", "Content-Type: application/json", "-d", body])
        .arg(format!("http://127.0.0.1:{port}{path}"))
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn ready(port: u16) -> bool {
    Command::new("curl")
        .args(["-s", "-m", "2", "-o", "/dev/null", "-w", "%{http_code}"])
        .arg(format!("http://127.0.0.1:{port}/health"))
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "200")
        .unwrap_or(false)
}

/// Delete all but the newest `MAX_FILES` state files.
fn prune(dir: &PathBuf) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).map_or(false, |n| n.starts_with("slipstream-") && n.ends_with(".bin")))
        .filter_map(|p| std::fs::metadata(&p).and_then(|m| m.modified()).ok().map(|t| (t, p)))
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, p) in files.into_iter().skip(MAX_FILES) {
        let _ = std::fs::remove_file(p);
    }
}

/// Ask a running, ready llama-server to write slot 0 to `key`. Best effort, bounded in time:
/// an empty slot, a busy server or low disk simply skip the save.
pub fn save(port: u16, key: &str) -> bool {
    if !ready(port) {
        return false;
    }
    let dir = slot_dir();
    // Room for a ~1 GB state while staying well above the engine's own disk reserve.
    if free_disk_gb(&dir).map_or(true, |g| g < MIN_FREE_AFTER_SAVE_GB) {
        return false;
    }
    let body = format!("{{\"filename\":\"{key}\"}}");
    let ok = post(port, "/slots/0?action=save", &body, 30)
        .map(|r| !r.contains("\"n_saved\":0"))
        .unwrap_or(false);
    if !ok {
        let _ = std::fs::remove_file(dir.join(key));
    }
    prune(&dir);
    ok
}

/// After a start: wait (in a thread) until the model is loaded, then restore `key` if it exists.
/// A file the server rejects (different setup, truncated) is deleted.
pub fn restore_in_background(port: u16, key: String) {
    let path = slot_dir().join(&key);
    if !path.exists() {
        return;
    }
    std::thread::spawn(move || {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(900) {
            if ready(port) {
                let body = format!("{{\"filename\":\"{key}\"}}");
                if post(port, "/slots/0?action=restore", &body, 60).is_none() {
                    let _ = std::fs::remove_file(&path);
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_stable_and_setup_specific() {
        let a = slot_key("/m/q.gguf", "/m/q.pgrn", 40960, "f16", 123);
        assert_eq!(a, slot_key("/m/q.gguf", "/m/q.pgrn", 40960, "f16", 123));
        assert!(a.starts_with("slipstream-") && a.ends_with(".bin"));
        assert_ne!(a, slot_key("/m/q.gguf", "/m/q.pgrn", 32768, "f16", 123));
        assert_ne!(a, slot_key("/m/q.gguf", "/m/q.pgrn", 40960, "q8_0", 123));
        assert_ne!(a, slot_key("/m/q.gguf", "/m/q.pgrn", 40960, "f16", 124));
        // llama-server only accepts plain file names
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.'));
    }

    #[test]
    fn fnv_known_value() {
        assert_eq!(fnv1a(""), 0xcbf29ce484222325);
        assert_eq!(fnv1a("a"), 0xaf63dc4c8601ec8c);
    }
}
