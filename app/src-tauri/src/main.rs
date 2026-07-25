#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::fs::OpenOptions;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use tauri::{State, Manager};

const LOG_PATH: &str = "/tmp/peregrine-control-server.log";
const DL_LOG: &str = "/tmp/peregrine-download.log";
const CONV_LOG: &str = "/tmp/peregrine-convert.log";
const EMB_LOG: &str = "/tmp/peregrine-embedder.log";
const QDRANT_LOG: &str = "/tmp/peregrine-qdrant.log";
const QSETUP_LOG: &str = "/tmp/peregrine-qdrant-setup.log";

const EMBED_URL: &str = "https://huggingface.co/nomic-ai/nomic-embed-text-v1.5-GGUF/resolve/main/nomic-embed-text-v1.5.Q5_K_M.gguf";
const QDRANT_URL: &str = "https://github.com/qdrant/qdrant/releases/download/v1.18.3/qdrant-aarch64-apple-darwin.tar.gz";

const SERVER_PORT: u16 = 8080;
const EMB_PORT: u16 = 8090;

#[derive(Default)]
struct AppState {
    server: Mutex<Option<Child>>,
    dl: Mutex<Option<Child>>,
    conv: Mutex<Option<Child>>,
    emb: Mutex<Option<Child>>,
    qdrant: Mutex<Option<Child>>,
    setup: Mutex<Option<Child>>,
}

/// HTTP status code of a GET (via curl), or 0 if the connection was refused.
/// Used to detect a live/ready server we may not own the child handle for.
fn http_status(url: &str) -> u16 {
    Command::new("curl")
        .args(["-s", "-m", "2", "-o", "/dev/null", "-w", "%{http_code}", url])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u16>().ok())
        .unwrap_or(0)
}

/// True if anything is listening on 127.0.0.1:port (llama.cpp answers /health
/// with 503 while loading, 200 when ready - both mean the port is taken).
fn port_alive(port: u16) -> bool {
    http_status(&format!("http://127.0.0.1:{port}/health")) != 0
}

fn num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn file_size(path: &str) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// True while the child in `slot` is alive; reaps it and returns false if it exited.
fn alive(slot: &Mutex<Option<Child>>) -> bool {
    let mut g = match slot.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    if let Some(c) = g.as_mut() {
        match c.try_wait() {
            Ok(Some(_)) => {
                *g = None;
                false
            }
            Ok(None) => true,
            Err(_) => true,
        }
    } else {
        false
    }
}

fn kill(slot: &Mutex<Option<Child>>) {
    if let Ok(mut g) = slot.lock() {
        if let Some(mut c) = g.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

// ---------------------------------------------------------------- server ----

#[derive(Deserialize)]
struct ServerConfig {
    server: String,
    model: String,
    pgrn: String,
    cache_gb: f64,
    headroom_gb: f64,
    ctx: u32,
    io_threads: u32,
    port: u16,
    thinking: bool,
    spec_type: String,   // "draft-mtp" (Qwen), "draft-dflash" (Laguna), or "none"
    draft_model: String, // draft gguf path (DFlash); empty for MTP/none
}

#[tauri::command]
fn start_server(cfg: ServerConfig, state: State<AppState>) -> Result<String, String> {
    let mut guard = state.server.lock().map_err(|e| e.to_string())?;
    if guard.is_some() {
        return Err("Server läuft bereits - erst stoppen.".into());
    }
    if file_size(&cfg.model) == 0 {
        return Err("Modell (.gguf) fehlt - erst herunterladen.".into());
    }
    if file_size(&cfg.pgrn) == 0 {
        return Err("PGRN-Sidecar fehlt - erst konvertieren.".into());
    }
    let log = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(LOG_PATH)
        .map_err(|e| format!("Log-Fehler: {e}"))?;
    let log_err = log.try_clone().map_err(|e| e.to_string())?;

    let mut cmd = Command::new(&cfg.server);
    cmd.args([
        "--model", &cfg.model,
        "--pgrn", &cfg.pgrn,
        "--pgrn-cache-gb", &num(cfg.cache_gb),
        "--pgrn-headroom-gb", &num(cfg.headroom_gb),
        "--gpu-layers", "99",
        "--ctx-size", &cfg.ctx.to_string(),
        "--parallel", "1",
        // Large prefill batch: more tokens share one SSD expert-fetch, so
        // reading big Kilo prompts is ~2.7x faster (measured 208 vs 75 tok/s).
        "--batch-size", "2048",
        "--ubatch-size", "2048",
        "-fa", "on",
        "-ctk", "q8_0",
        "-ctv", "q8_0",
        "--jinja",
        "--alias", "slipstream",
        "--host", "127.0.0.1",
        "--port", &cfg.port.to_string(),
        "--no-warmup",
    ]);
    if cfg.io_threads > 1 {
        cmd.arg("--pgrn-io-threads").arg(cfg.io_threads.to_string());
    }
    // Speculative decoding is model-specific: Qwen ships an MTP head (no draft
    // file); Laguna uses a separate DFlash draft. "none" disables it.
    if cfg.spec_type != "none" && !cfg.spec_type.is_empty() {
        cmd.args(["--spec-type", &cfg.spec_type, "--spec-draft-n-max", "4"]);
        if !cfg.draft_model.is_empty() && file_size(&cfg.draft_model) > 0 {
            cmd.args(["--model-draft", &cfg.draft_model, "--spec-draft-ngl", "99"]);
        }
    }
    // Couple sampler to thinking mode. Thinking-on uses Qwen3's recommended
    // sampler AND a bounded reasoning budget, so the model can't over-think into
    // an endless-looking chain that never answers. Coding (thinking off) stays
    // deterministic and skips reasoning entirely.
    if cfg.thinking {
        cmd.args([
            "--temp", "0.6", "--top-p", "0.95", "--top-k", "20",
            "--reasoning-budget", "2048",
        ]);
    } else {
        cmd.args(["--reasoning", "off", "--temp", "0"]);
    }
    cmd.stdout(Stdio::from(log)).stderr(Stdio::from(log_err));
    let child = cmd.spawn().map_err(|e| format!("Start fehlgeschlagen: {e}"))?;
    *guard = Some(child);
    Ok("Server gestartet - lädt Modell (~60s)...".into())
}

#[tauri::command]
fn stop_server(state: State<AppState>) -> Result<String, String> {
    kill(&state.server);
    // Fallback: also reap a server we may not own the handle for (e.g. one
    // spawned by a previous app instance) so the port frees up.
    if port_alive(SERVER_PORT) {
        let _ = Command::new("pkill")
            .args(["-f", &format!("llama-server.*--port {SERVER_PORT}")])
            .status();
    }
    Ok("Server gestoppt.".into())
}

#[tauri::command]
fn is_running(state: State<AppState>) -> bool {
    alive(&state.server) || port_alive(SERVER_PORT)
}

/// "down" | "loading" | "ready" - based on a real /health probe, so it is
/// correct even for a server this app instance did not spawn.
#[tauri::command]
fn server_state() -> String {
    match http_status(&format!("http://127.0.0.1:{SERVER_PORT}/health")) {
        200 => "ready".into(),
        0 => "down".into(),
        _ => "loading".into(),
    }
}

#[tauri::command]
fn read_log(max_lines: usize) -> String {
    let s = std::fs::read_to_string(LOG_PATH).unwrap_or_default();
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

// ------------------------------------------------------------- sys stats ----

#[derive(Serialize)]
struct SysStats {
    free_gib: f64,
    swap_used_mb: f64,
    total_gib: f64,
    cores: f64,       // total physical cores
    perf_cores: f64,  // Apple Silicon performance (P-) cores -> good proxy for parallel I/O width
}

fn sysctl(key: &str) -> Option<String> {
    let out = Command::new("sysctl").arg("-n").arg(key).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[tauri::command]
fn system_stats() -> SysStats {
    let free_gib = (|| {
        let out = Command::new("vm_stat").output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout);
        let get = |k: &str| -> f64 {
            s.lines()
                .find(|l| l.contains(k))
                .and_then(|l| l.split_whitespace().last())
                .and_then(|v| v.trim_end_matches('.').parse::<f64>().ok())
                .unwrap_or(0.0)
        };
        Some((get("Pages free:") + get("Pages inactive:")) * 16384.0 / 1_073_741_824.0)
    })()
    .unwrap_or(0.0);
    let swap_used_mb = sysctl("vm.swapusage")
        .and_then(|s| s.split("used =").nth(1)?.trim().split('M').next()?.trim().parse::<f64>().ok())
        .unwrap_or(0.0);
    let total_gib = sysctl("hw.memsize")
        .and_then(|s| s.parse::<f64>().ok())
        .map(|b| b / 1_073_741_824.0)
        .unwrap_or(0.0);
    let cores = sysctl("hw.physicalcpu").and_then(|s| s.parse::<f64>().ok()).unwrap_or(8.0);
    let perf_cores = sysctl("hw.perflevel0.physicalcpu")
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(cores); // Intel Macs lack perflevel0 -> fall back to all physical cores
    SysStats { free_gib, swap_used_mb, total_gib, cores, perf_cores }
}

// --------------------------------------------------------- model manager ----

#[derive(Serialize)]
struct ModelStatus {
    gguf_bytes: u64,
    pgrn_bytes: u64,
    downloading: bool,
    converting: bool,
    disk_free_gib: f64,
}

fn disk_free_gib(dir: &str) -> f64 {
    // parse `df -k <dir>`: last row, 4th column = available KiB
    let probe = if std::path::Path::new(dir).exists() { dir } else { "/" };
    (|| {
        let out = Command::new("df").arg("-k").arg(probe).output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout);
        let last = s.lines().last()?;
        let avail_kb: f64 = last.split_whitespace().nth(3)?.parse().ok()?;
        Some(avail_kb * 1024.0 / 1_073_741_824.0)
    })()
    .unwrap_or(0.0)
}

#[tauri::command]
fn model_status(gguf: String, pgrn: String, dir: String, state: State<AppState>) -> ModelStatus {
    ModelStatus {
        gguf_bytes: file_size(&gguf),
        pgrn_bytes: file_size(&pgrn),
        downloading: alive(&state.dl),
        converting: alive(&state.conv),
        disk_free_gib: disk_free_gib(&dir),
    }
}

/// Content-Length of the (redirect-followed) URL, or 0.
#[tauri::command]
fn remote_size(url: String) -> u64 {
    (|| {
        let out = Command::new("curl").args(["-sIL", &url]).output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout);
        s.lines()
            .filter(|l| l.to_lowercase().starts_with("content-length:"))
            .filter_map(|l| l.split(':').nth(1)?.trim().parse::<u64>().ok())
            .max()
    })()
    .unwrap_or(0)
}

#[tauri::command]
fn start_download(url: String, dest: String, dir: String, state: State<AppState>) -> Result<String, String> {
    if alive(&state.dl) {
        return Err("Download läuft bereits.".into());
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("Ordner: {e}"))?;
    let log = OpenOptions::new().create(true).write(true).truncate(true).open(DL_LOG)
        .map_err(|e| e.to_string())?;
    let child = Command::new("curl")
        .args(["-L", "-C", "-", "--fail", "--retry", "5", "-o", &dest, &url])
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|e| format!("curl-Start: {e}"))?;
    *state.dl.lock().map_err(|e| e.to_string())? = Some(child);
    Ok("Download gestartet.".into())
}

#[tauri::command]
fn cancel_download(state: State<AppState>) {
    kill(&state.dl);
}

#[tauri::command]
fn start_convert(repo: String, gguf: String, pgrn: String, state: State<AppState>) -> Result<String, String> {
    if alive(&state.conv) {
        return Err("Konvertierung läuft bereits.".into());
    }
    if file_size(&gguf) == 0 {
        return Err("GGUF fehlt - erst herunterladen.".into());
    }
    let python = format!("{repo}/.venv/bin/python");
    let log = OpenOptions::new().create(true).write(true).truncate(true).open(CONV_LOG)
        .map_err(|e| e.to_string())?;
    let child = Command::new(&python)
        .args(["-m", "bench.m1.convert_gguf_to_pgrn", "--input", &gguf, "--output", &pgrn, "--min-free-gb", "8"])
        .current_dir(&repo)
        .env("PYTHONPATH", &repo)
        .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|e| format!("Python-Start: {e} (venv unter {python}?)"))?;
    *state.conv.lock().map_err(|e| e.to_string())? = Some(child);
    Ok("Konvertierung gestartet.".into())
}

#[tauri::command]
fn cancel_convert(state: State<AppState>) {
    kill(&state.conv);
}

#[tauri::command]
fn tail_file(path: String, max_lines: usize) -> String {
    let s = std::fs::read_to_string(&path).unwrap_or_default();
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

// ------------------------------------------------------------- indexing ----
// Embedder = our llama-server in --embedding mode (nomic-embed). Qdrant = the
// upstream release binary. Both downloadable + startable from the app so Kilo
// codebase indexing works fully local.

#[derive(Deserialize)]
struct EmbConfig {
    server: String,
    model: String,
}

#[tauri::command]
fn start_embedder(cfg: EmbConfig, state: State<AppState>) -> Result<String, String> {
    if alive(&state.emb) || port_alive(EMB_PORT) {
        return Err("Embedder laeuft bereits.".into());
    }
    if file_size(&cfg.model) == 0 {
        return Err("Embed-Modell fehlt - erst herunterladen.".into());
    }
    let log = OpenOptions::new().create(true).write(true).truncate(true).open(EMB_LOG)
        .map_err(|e| e.to_string())?;
    let errlog = log.try_clone().map_err(|e| e.to_string())?;
    let child = Command::new(&cfg.server)
        .args([
            "--model", &cfg.model,
            "--embedding",
            "--pooling", "mean",
            "--ctx-size", "2048",
            "--batch-size", "2048",
            "--ubatch-size", "2048",
            "-ngl", "99",
            "--alias", "nomic-embed-text",
            "--host", "127.0.0.1",
            "--port", &EMB_PORT.to_string(),
        ])
        .stdout(Stdio::from(log)).stderr(Stdio::from(errlog))
        .spawn().map_err(|e| format!("Embedder-Start: {e}"))?;
    *state.emb.lock().map_err(|e| e.to_string())? = Some(child);
    Ok("Embedder gestartet (Port 8090).".into())
}

#[tauri::command]
fn stop_embedder(state: State<AppState>) {
    kill(&state.emb);
    if port_alive(EMB_PORT) {
        let _ = Command::new("pkill")
            .args(["-f", &format!("llama-server.*--port {EMB_PORT}")]).status();
    }
}

/// Download + extract the Qdrant release binary into `dir` (dir/qdrant).
#[tauri::command]
fn install_qdrant(dir: String, state: State<AppState>) -> Result<String, String> {
    if alive(&state.setup) {
        return Err("Installation laeuft bereits.".into());
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("Ordner: {e}"))?;
    let log = OpenOptions::new().create(true).write(true).truncate(true).open(QSETUP_LOG)
        .map_err(|e| e.to_string())?;
    let errlog = log.try_clone().map_err(|e| e.to_string())?;
    let script = format!(
        "set -e; cd '{dir}'; curl -L --fail --retry 5 -o qdrant.tar.gz '{QDRANT_URL}'; \
         tar xzf qdrant.tar.gz; rm -f qdrant.tar.gz; chmod +x qdrant; echo INSTALL_DONE"
    );
    let child = Command::new("sh").arg("-c").arg(script)
        .stdout(Stdio::from(log)).stderr(Stdio::from(errlog))
        .spawn().map_err(|e| format!("Setup-Start: {e}"))?;
    *state.setup.lock().map_err(|e| e.to_string())? = Some(child);
    Ok("Qdrant-Download gestartet.".into())
}

#[tauri::command]
fn start_qdrant(bin: String, storage: String, state: State<AppState>) -> Result<String, String> {
    if alive(&state.qdrant) || port_alive(6333) {
        return Err("Qdrant laeuft bereits.".into());
    }
    if file_size(&bin) == 0 {
        return Err("Qdrant-Binary fehlt - erst installieren.".into());
    }
    std::fs::create_dir_all(&storage).map_err(|e| format!("Storage: {e}"))?;
    let dir = std::path::Path::new(&bin).parent()
        .map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| ".".into());
    let log = OpenOptions::new().create(true).write(true).truncate(true).open(QDRANT_LOG)
        .map_err(|e| e.to_string())?;
    let errlog = log.try_clone().map_err(|e| e.to_string())?;
    let child = Command::new(&bin)
        .current_dir(&dir)
        .env("QDRANT__STORAGE__STORAGE_PATH", &storage)
        .env("QDRANT__SERVICE__HOST", "127.0.0.1")
        .stdout(Stdio::from(log)).stderr(Stdio::from(errlog))
        .spawn().map_err(|e| format!("Qdrant-Start: {e}"))?;
    *state.qdrant.lock().map_err(|e| e.to_string())? = Some(child);
    Ok("Qdrant gestartet (Port 6333).".into())
}

#[tauri::command]
fn stop_qdrant(state: State<AppState>) {
    kill(&state.qdrant);
    if port_alive(6333) {
        let _ = Command::new("pkill").args(["-f", "/qdrant"]).status();
    }
}

#[derive(Serialize)]
struct IndexStatus {
    emb_bytes: u64,
    emb_state: String, // down | loading | ready
    qdrant_installed: bool,
    qdrant_running: bool,
    installing: bool,
}

#[tauri::command]
fn index_status(emb_model: String, qdrant_bin: String, state: State<AppState>) -> IndexStatus {
    let emb_state = match http_status(&format!("http://127.0.0.1:{EMB_PORT}/health")) {
        200 => "ready",
        0 => "down",
        _ => "loading",
    }
    .to_string();
    IndexStatus {
        emb_bytes: file_size(&emb_model),
        emb_state,
        qdrant_installed: file_size(&qdrant_bin) > 0,
        qdrant_running: alive(&state.qdrant) || port_alive(6333),
        installing: alive(&state.setup),
    }
}

// -------------------------------------------------------------- defaults ----

#[derive(Serialize)]
struct Defaults {
    home: String,
    repo: String,
    server_bin: String,
    model_dir: String,
    embed_url: String,
    embed_file: String,
}

/// Sensible starting paths so the UI is pre-filled but everything stays
/// editable (and pickable) by the user.
#[tauri::command]
fn defaults(app: tauri::AppHandle) -> Defaults {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".into());
    // The repo that ships this app (dev layout); overridable in the UI.
    let repo = concat!(env!("CARGO_MANIFEST_DIR"), "/../../..");
    let repo = std::fs::canonicalize(repo)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| format!("{home}/LLM-BOOM"));
    // Prefer the self-contained engine bundled inside the .app (works on any Mac);
    // fall back to the dev build dir when running from the repo.
    let server_bin = app
        .path()
        .resource_dir()
        .ok()
        .and_then(|d| {
            ["resources/llama-server", "llama-server"]
                .iter()
                .map(|c| d.join(c))
                .find(|p| p.exists())
        })
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("{repo}/vendor/llama.cpp/build/bin/llama-server"));
    let model_dir = format!("{home}/Modelle");
    Defaults {
        home,
        repo,
        server_bin,
        model_dir,
        embed_url: EMBED_URL.into(),
        embed_file: "nomic-embed-text-v1.5.Q5_K_M.gguf".into(),
    }
}

// ------------------------------------------------------------ kilo patch ----

#[derive(Deserialize)]
struct KiloConfig {
    home: String,
    base_url: String,
    model: String, // bare model id, e.g. "slipstream"
    api_key: String,
    ctx: u32,
    max_out: u32,
    #[serde(default)]
    target: String, // "kilo" (default) or "opencode" - same app.kilo.ai schema, different path
}

/// Patch an agent's global config in place, using the app.kilo.ai / opencode
/// schema (provider + model). Non-destructive: other top-level keys (mcp,
/// permission, ...) are preserved. Kilo + OpenCode share this schema; the user
/// just restarts the editor afterwards - no export/import copy-paste.
#[tauri::command]
fn patch_kilo_config(cfg: KiloConfig) -> Result<String, String> {
    let (dir, fname) = match cfg.target.as_str() {
        "opencode" => (format!("{}/.config/opencode", cfg.home), "opencode.json"),
        _ => (format!("{}/.config/kilo", cfg.home), "kilo.jsonc"),
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("Ordner: {e}"))?;
    let path = format!("{dir}/{fname}");

    // Reuse the existing config if it parses as a JSON object; else start fresh.
    // (A file with // comments won't parse - we then write a clean valid one.)
    let mut root: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}));
    let obj = root.as_object_mut().unwrap();

    let mut model_info = Map::new();
    model_info.insert("name".into(), json!("Peregrine 35B (local SSD-streaming)"));
    model_info.insert("tool_call".into(), json!(true));
    model_info.insert("limit".into(), json!({ "context": cfg.ctx, "output": cfg.max_out }));

    let mut models = Map::new();
    models.insert(cfg.model.clone(), Value::Object(model_info));

    let provider_entry = json!({
        "npm": "@ai-sdk/openai-compatible",
        "name": "Peregrine Local",
        "options": { "baseURL": cfg.base_url, "apiKey": cfg.api_key },
        "models": Value::Object(models),
    });
    let mut provider = Map::new();
    provider.insert("peregrine".into(), provider_entry);

    obj.insert("$schema".into(), json!("https://app.kilo.ai/config.json"));
    obj.insert("model".into(), json!(format!("peregrine/{}", cfg.model)));
    obj.insert("provider".into(), Value::Object(provider));
    obj.entry("permission").or_insert_with(|| json!({ "bash": "allow" }));
    // Enable Kilo's codebase search (documented schema key). The embedder /
    // Qdrant values themselves live in Kilo's Settings UI, so we only flip the
    // valid flag here and don't inject uncertain keys that could invalidate.
    let exp = obj.entry("experimental").or_insert_with(|| json!({}));
    if let Some(e) = exp.as_object_mut() {
        e.insert("codebase_search".into(), json!(true));
    } else {
        *exp = json!({ "codebase_search": true });
    }

    let out = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    std::fs::write(&path, out + "\n").map_err(|e| format!("Schreiben: {e}"))?;
    Ok(path)
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            start_server, stop_server, is_running, server_state, read_log, system_stats,
            model_status, remote_size, start_download, cancel_download,
            start_convert, cancel_convert, tail_file, patch_kilo_config, defaults,
            start_embedder, stop_embedder, install_qdrant, start_qdrant, stop_qdrant, index_status
        ])
        .run(tauri::generate_context!())
        .expect("error while running Peregrine Control");
}
