//! Keep the agent's prompt state across server restarts.
//!
//! A 40k-token coding-agent context takes ~2-3 minutes to read on a 36 GB Mac. llama-server keeps
//! it in RAM between turns, but every restart (settings change, app quit) threw it away. Before the
//! server stops we ask it to write slot 0 to disk (`/slots/0?action=save`, KV + recurrent state +
//! context checkpoints); after the next start, once the model is loaded, we restore it. The next
//! request then only reads its new tokens. A restored state is the same state the server had, so
//! answers are identical (bench E5).
//!
//! Safety rules (review 2026-11-28):
//! - the file key covers everything that shapes the state, including the engine binary, so a
//!   different model, quant, context, KV type or engine build never loads a foreign state;
//! - a restore thread belongs to one server start (generation counter) and never fires into a
//!   later server;
//! - a good file is only replaced by a save that actually wrote a comparable state (written to a
//!   temp name first; a much smaller state never replaces a big agent context) and only deleted
//!   when the engine explicitly refuses it — never on a timeout or a busy server;
//! - saving is skipped (fast) while the server is generating, so Stop/Quit never wait for it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Never let saved prompt states eat the disk the engine needs (PGRN admission keeps ~16 GB free).
const MIN_FREE_BEFORE_SAVE_GB: f64 = 24.0;
/// How many different setups keep a saved state (newest wins).
const MAX_FILES: usize = 2;
/// A new save replaces the existing file only if it holds at least this share of its tokens.
const MIN_TOKEN_SHARE: f64 = 0.5;

/// Bumped on every server start and stop; a restore thread only acts for its own generation.
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn bump_generation() -> u64 {
    GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}

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

/// Size + mtime of a file, so a re-downloaded model or an updated engine changes the key.
pub fn file_stamp(path: &str) -> String {
    std::fs::metadata(path)
        .map(|m| {
            let t = m.modified().ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("{}:{}", m.len(), t)
        })
        .unwrap_or_default()
}

/// File name for a server setup (model, sidecar, context, KV type, model file, engine binary).
pub fn slot_key(model: &str, pgrn: &str, ctx: u32, kv_quant: &str, model_stamp: &str, server_stamp: &str) -> String {
    let id = format!("{model}|{pgrn}|{ctx}|{kv_quant}|{model_stamp}|{server_stamp}");
    format!("slipstream-{:016x}.bin", fnv1a(&id))
}

fn free_disk_gb(dir: &Path) -> Option<f64> {
    let out = Command::new("df").args(["-k"]).arg(dir).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let avail_kb: f64 = s.lines().nth(1)?.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail_kb / 1_048_576.0)
}

/// POST returning (http status, body); status 0 = no answer (timeout, refused).
fn post(port: u16, path: &str, body: &str, timeout_s: u32) -> (u16, String) {
    let out = Command::new("curl")
        .args(["-s", "-m", &timeout_s.to_string(), "-X", "POST",
               "-H", "Content-Type: application/json", "-d", body,
               "-w", "\n%{http_code}"])
        .arg(format!("http://127.0.0.1:{port}{path}"))
        .output();
    let Ok(out) = out else { return (0, String::new()) };
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    let (body, code) = s.rsplit_once('\n').unwrap_or(("", s.as_str()));
    (code.trim().parse().unwrap_or(0), body.to_string())
}

fn get(port: u16, path: &str) -> Option<String> {
    let out = Command::new("curl")
        .args(["-s", "--fail", "-m", "2"])
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

/// True if slot 0 is working on a request (then a save/restore would be deferred by the server).
fn slot_busy(port: u16) -> bool {
    get(port, "/slots")
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.as_array().and_then(|a| a.first().cloned()))
        .and_then(|s| s.get("is_processing").and_then(|b| b.as_bool()))
        .unwrap_or(true)
}

fn json_u64(body: &str, key: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(body).ok()?.get(key)?.as_u64()
}

fn meta_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.tokens"))
}

fn saved_tokens(dir: &Path, key: &str) -> u64 {
    std::fs::read_to_string(meta_path(dir, key)).ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Delete all but the newest `MAX_FILES` state files (and their token notes).
fn prune(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).map_or(false, |n| n.starts_with("slipstream-") && n.ends_with(".bin") && !n.contains(".tmp")))
        .filter_map(|p| std::fs::metadata(&p).and_then(|m| m.modified()).ok().map(|t| (t, p)))
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, p) in files.into_iter().skip(MAX_FILES) {
        let _ = std::fs::remove_file(&p);
        if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
            let _ = std::fs::remove_file(meta_path(dir, n));
        }
    }
}

/// Decide whether a fresh save of `new_tokens` may replace a file holding `old_tokens`.
fn should_replace(new_tokens: u64, old_tokens: u64) -> bool {
    new_tokens > 0 && (new_tokens as f64) >= MIN_TOKEN_SHARE * old_tokens as f64
}

/// Ask a running, idle llama-server to write slot 0. Best effort and quick (≤ ~10 s worst case,
/// ~1 s normally): a busy or loading server, low disk or a small state skip the save and keep
/// the existing file.
pub fn save(port: u16, key: &str) -> bool {
    if !ready(port) || slot_busy(port) {
        return false;
    }
    let dir = slot_dir();
    if free_disk_gb(&dir).map_or(true, |g| g < MIN_FREE_BEFORE_SAVE_GB) {
        return false;
    }
    let tmp = key.replace(".bin", ".tmp.bin");
    let (code, body) = post(port, "/slots/0?action=save", &format!("{{\"filename\":\"{tmp}\"}}"), 8);
    let n = if code == 200 { json_u64(&body, "n_saved").unwrap_or(0) } else { 0 };
    let ok = should_replace(n, saved_tokens(&dir, key))
        && std::fs::rename(dir.join(&tmp), dir.join(key)).is_ok();
    if ok {
        let _ = std::fs::write(meta_path(&dir, key), n.to_string());
    } else {
        let _ = std::fs::remove_file(dir.join(&tmp));
    }
    prune(&dir);
    ok
}

/// After a start: wait (in a thread) until the model is loaded, then restore `key` if it exists
/// and this start is still the current one. Only an explicit refusal by the engine deletes the
/// file; timeouts and a busy slot keep it for next time.
pub fn restore_in_background(port: u16, key: String, generation: u64) {
    let dir = slot_dir();
    if !dir.join(&key).exists() {
        return;
    }
    std::thread::spawn(move || {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(900) {
            if GENERATION.load(Ordering::SeqCst) != generation {
                return;
            }
            if ready(port) {
                if slot_busy(port) || GENERATION.load(Ordering::SeqCst) != generation {
                    return; // a request got there first: keep its work, keep the file
                }
                let (code, body) = post(port, "/slots/0?action=restore", &format!("{{\"filename\":\"{key}\"}}"), 20);
                if code == 400 && body.contains("Unable to restore") {
                    let _ = std::fs::remove_file(dir.join(&key));
                    let _ = std::fs::remove_file(meta_path(&dir, &key));
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_stable_and_setup_specific() {
        let k = |ctx, kv, m, s| slot_key("/m/q.gguf", "/m/q.pgrn", ctx, kv, m, s);
        let a = k(40960, "f16", "1:2", "3:4");
        assert_eq!(a, k(40960, "f16", "1:2", "3:4"));
        assert!(a.starts_with("slipstream-") && a.ends_with(".bin"));
        assert_ne!(a, k(32768, "f16", "1:2", "3:4"));
        assert_ne!(a, k(40960, "q8_0", "1:2", "3:4"));
        assert_ne!(a, k(40960, "f16", "1:9", "3:4"), "re-downloaded model");
        assert_ne!(a, k(40960, "f16", "1:2", "3:9"), "updated engine binary");
        // llama-server only accepts plain file names
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.'));
        assert!(a.replace(".bin", ".tmp.bin").chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.'));
    }

    #[test]
    fn small_state_never_replaces_agent_context() {
        assert!(should_replace(39_564, 0));
        assert!(should_replace(39_600, 39_564));
        assert!(should_replace(25_000, 39_564));
        assert!(!should_replace(300, 39_564), "a title request must not overwrite the agent context");
        assert!(!should_replace(0, 0));
    }

    #[test]
    fn generation_moves_forward() {
        let a = bump_generation();
        let b = bump_generation();
        assert!(b > a);
    }

    #[test]
    fn fnv_known_value() {
        assert_eq!(fnv1a(""), 0xcbf29ce484222325);
        assert_eq!(fnv1a("a"), 0xaf63dc4c8601ec8c);
    }
}
