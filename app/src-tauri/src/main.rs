#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::fs::OpenOptions;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

mod mlx;
mod p2p;
mod runtime;
mod servestats;
mod storage;
mod sysstats;
mod teardown;
mod tray;

use tauri::{Manager, State};

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

/// The readings the menubar poller takes, held where the window can read them
/// too. One sampler for both means the menu and the Status panel cannot disagree,
/// and that opening a panel costs no extra `/metrics` request.
struct Live {
    serving: Mutex<servestats::Store>,
    system: Mutex<sysstats::SysSnapshot>,
    /// oMLX `/api/status` extras (PGRN hit-rate, RSS) — filled by the tray poller.
    api_extras: Mutex<servestats::ApiExtras>,
}

/// Everything the Status panel draws, in one call.
#[tauri::command]
fn live_stats(live: State<Live>, state: State<AppState>) -> Value {
    // Hold Live mutexes only long enough to copy. Expert-cache parsing and any
    // other I/O stays outside — the tray poller used to nest HTTP under these
    // same locks and hang the window (main thread waiting on live_stats).
    let (system, session, alltime, serving_available, extras) = {
        let serving = live
            .serving
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let system = live
            .system
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let extras = live
            .api_extras
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        (
            system,
            serving.session(),
            serving.alltime(),
            serving.has_reading(),
            extras,
        )
    };
    // Metal: last decode line from the engine log. MLX: lifetime avg from /api/status
    // when the log has no eval-time / output= line.
    let last_tps = last_tps().or(extras.avg_generation_tps);
    let experts = tray::expert_cache(&state).or_else(|| {
        extras.has_pgrn().then(|| tray::ExpertCache {
            hits: extras.pgrn_hits.unwrap_or(0),
            misses: extras.pgrn_misses.unwrap_or(0),
            hit_rate: extras.pgrn_hit_rate.unwrap_or(0.0),
        })
    });
    // Prefer oMLX-reported RSS; else `ps` on the child we own (Metal path has no
    // /api/status). Never invent a number — UI shows "–" when both are absent.
    let process_rss = extras
        .process_rss_bytes
        .or_else(|| server_child_rss_bytes(&state));
    let (rss_bytes, rss_source) = match (process_rss, extras.model_memory_bytes) {
        (Some(b), _) => (Some(b), Some("process")),
        (None, Some(b)) => (Some(b), Some("model_memory")),
        _ => (None, None),
    };
    json!({
        "system": system,
        "session": session,
        "alltime": alltime,
        // False until a server answered /metrics: the panel then says so instead
        // of drawing a row of zeroes that look like a measurement.
        "serving_available": serving_available,
        // Hit rate and misses in one object: the window draws the rate and derives
        // SSD throughput from the miss delta, both off this single parse.
        "experts": experts,
        // Last completed decode (Metal/oMLX log) or oMLX avg_generation_tps fallback.
        "last_tps": last_tps,
        "model_memory_bytes": extras.model_memory_bytes,
        "process_rss_bytes": process_rss,
        // Unified RSS for the status strip: process preferred, model_memory fallback.
        "rss_bytes": rss_bytes,
        "rss_source": rss_source,
        "pgrn_high_water_bytes": extras.pgrn_high_water_bytes,
        "pgrn_mx_size": extras.pgrn_mx_size,
    })
}

/// Current RSS of the server child this app spawned (`ps` KiB → bytes).
/// None when we do not own the process or `ps` fails — honest empty for UI.
fn server_child_rss_bytes(state: &AppState) -> Option<u64> {
    let pid = {
        let guard = state.server.lock().ok()?;
        guard.as_ref().map(|c| c.id())?
    };
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let kib: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    (kib > 0).then_some(kib.saturating_mul(1024))
}

/// Resets one scope. The session is ours to offset; the all-time total is ours to
/// delete. Neither touches the running server.
#[tauri::command]
fn clear_stats(scope: String, live: State<Live>) -> Result<(), String> {
    let mut serving = live
        .serving
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match scope.as_str() {
        "session" => serving.clear_session(),
        "alltime" => {
            serving.clear_alltime();
            serving.persist();
        }
        other => return Err(format!("unknown scope: {other}")),
    }
    Ok(())
}

#[tauri::command]
fn runtime_preflight(app: tauri::AppHandle) -> Result<runtime::RuntimeReport, String> {
    let root = app
        .path()
        .resource_dir()
        .map_err(|e| e.to_string())?
        .join("resources");
    runtime::preflight(&root)
}

#[tauri::command]
fn inspect_storage(
    path: String,
    role: String,
    planned_bytes: u64,
    reserve_bytes: u64,
) -> Result<storage::StorageReport, String> {
    storage::inspect_storage(
        std::path::Path::new(path.trim()),
        role.trim(),
        planned_bytes,
        reserve_bytes,
    )
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

/// Body of a GET, or None if the request failed. Same curl route as
/// `http_status` so there is one HTTP mechanism in this app, not two.
fn http_body(url: &str) -> Option<String> {
    let out = Command::new("curl")
        .args(["-s", "-m", "2", "--fail", url])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
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
    // SIGTERM first so launcher EXIT traps (oMLX lock release) can run.
    let _ = teardown::kill_child_graceful(slot, 2000);
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
    #[serde(default)]
    pgrn_mirror: String, // advanced: byte-identical PGRN copy on a 2nd SSD -> dual-SSD striping
    #[serde(default)]
    pgrn_buffered: bool, // advanced: buffered reads (skip F_NOCACHE) for non-NVMe drives
    #[serde(default)]
    pgrn_online: bool,   // advanced: online co-activation predictor -> speculative prefetch
    #[serde(default = "default_true")]
    pgrn_compact: bool,  // default-on: zero-copy compact slots -> +13-24% decode at moderate cache, swap-safe (measured)
    #[serde(default = "default_true")]
    grammar_draft: bool, // default-on: grammar-forced drafts for structured output (JSON/tool-calls); adaptive-guarded + lossless. Measured +45% tok/s fetch-bound on rigid schemas, neutral on easy JSON (guard stands down)
    #[serde(default = "default_kv_quant")]
    kv_quant: String, // per-model KV type: "q8_0" for full-attention models (KV-RAM becomes cache headroom), "f16" for hybrids (S1 measured q8-KV at -12..-28% decode on the resident hybrid 35B; its linear-attention KV is tiny, so q8 buys nothing there)
    /// "metal" | "mlx" | "auto" | "heuristic". Auto/heuristic resolved at start (see mlx::resolve_backend).
    #[serde(default = "default_backend")]
    backend: String,
    /// Parent directory of MLX model subdirs (`--model-dir`). Empty → default SSD path.
    #[serde(default)]
    mlx_dir: String,
    /// Optional absolute path to the `omlx` binary.
    #[serde(default)]
    omlx_bin: String,
    /// Estimated prompt size (chars) for Auto hybrid: long → Metal, short/warm → MLX.
    #[serde(default)]
    prompt_chars: usize,
    /// MLX-only: `SLIPSTREAM_PGRN_PROFILE` (`balanced` | `quality` | `fast`).
    #[serde(default = "default_pgrn_profile")]
    pgrn_profile: String,
    /// MLX-only: `SLIPSTREAM_PGRN_RESIDENCY` (`mlock` | `touch` | `off`).
    #[serde(default = "default_pgrn_residency")]
    pgrn_residency: String,
    /// MLX-only: `SLIPSTREAM_PGRN_KEEP_HOT` (default on).
    #[serde(default = "default_true")]
    pgrn_keep_hot: bool,
    /// MLX-only: `SLIPSTREAM_PGRN_WARMUP` (default on).
    #[serde(default = "default_true")]
    pgrn_warmup: bool,
    /// MLX-only: when set → `SLIPSTREAM_PGRN_L3=peer` + `PEER_BASE`. Metal ignores.
    #[serde(default)]
    pgrn_l3_peer_base: String,
    /// MLX-only: path to MCP JSON/YAML → `OMLX_MCP_CONFIG` + `--mcp-config`.
    /// Empty (default) → MCP OFF (no server-side tool merge).
    #[serde(default)]
    mcp_config: String,
    /// MLX-only: Settings opt-in → `--memory-guard off` (Metal wired ~28 GiB escape).
    /// Default false → `--memory-guard-gb` = total − headroom.
    #[serde(default)]
    memory_guard_off: bool,
}

fn annotate_auto_backend(preference: &str, effective: &str, msg: String) -> String {
    if matches!(preference, "auto" | "heuristic") {
        format!("Auto → {effective}: {msg}")
    } else {
        msg
    }
}

fn default_true() -> bool { true }
fn default_kv_quant() -> String { "q8_0".into() }
fn default_backend() -> String { "auto".into() }
fn default_pgrn_profile() -> String { "balanced".into() }
fn default_pgrn_residency() -> String { "touch".into() }

#[tauri::command]
fn start_server(app: tauri::AppHandle, cfg: ServerConfig, state: State<AppState>) -> Result<String, String> {
    let mut guard = state.server.lock().map_err(|e| e.to_string())?;
    if guard.is_some() {
        return Err("Server läuft bereits - erst stoppen.".into());
    }
    let mlx_model_dir = {
        let trimmed = cfg.mlx_dir.trim();
        if trimmed.is_empty() {
            mlx::default_model_dir()
        } else {
            std::path::PathBuf::from(trimmed)
        }
    };
    let has_experts = mlx::any_experts_sidecar(&mlx_model_dir);
    let effective = mlx::resolve_backend(&cfg.backend, cfg.prompt_chars, has_experts);
    if effective == "mlx" {
        let msg = start_mlx_server(&app, &cfg, &mut *guard)?;
        return Ok(annotate_auto_backend(&cfg.backend, effective, msg));
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
    // Advanced I/O levers (opt-in): dual-SSD striping + buffered reads. The engine
    // reads these env vars; both default-off, parity-safe (mirror is CRC-checked).
    if !cfg.pgrn_mirror.trim().is_empty() {
        cmd.env("PGRN_MIRROR", cfg.pgrn_mirror.trim());
    }
    if cfg.pgrn_buffered {
        cmd.env("PGRN_BUFFERED", "1");
    }
    if cfg.pgrn_online {
        cmd.env("PGRN_ONLINE_PREDICT", "1");
    }
    // Compact slots (zero-copy): GPU reads experts directly from arena slots — no re-upload
    // copy. Measured +13-24% decode across cache 4-8 GiB, neutral higher, swap-safe. Default-on.
    if cfg.pgrn_compact {
        cmd.arg("--pgrn-compact-slots");
    }
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
        "--jinja",
        "--alias", "slipstream",
        "--host", "127.0.0.1",
        "--port", &cfg.port.to_string(),
        "--no-warmup",
        // Serving counters for the menubar's Serving Stats submenu.
        "--metrics",
    ]);
    // KV type per model family: f16 = engine default (skip the flags entirely).
    if !cfg.kv_quant.is_empty() && cfg.kv_quant != "f16" {
        cmd.args(["-ctk", &cfg.kv_quant, "-ctv", &cfg.kv_quant]);
    }
    // Width-weighted cache partition: auto-enable when a weights sidecar sits next
    // to the PGRN (bench.m1.partition_weights). Engine is fail-soft (equal split on
    // any file problem). Measured 2026-07-27 on the streamed 35B: +11% warm decode,
    // output byte-identical.
    if let Some(weights) = std::path::Path::new(&cfg.pgrn)
        .parent()
        .map(|dir| dir.join("partition-weights.txt"))
        .filter(|p| p.exists())
    {
        cmd.arg("--pgrn-partition-weights").arg(weights);
    }
    if cfg.io_threads > 1 {
        cmd.arg("--pgrn-io-threads").arg(cfg.io_threads.to_string());
    }
    // Speculative decoding is model-specific: Qwen Q4 ships an MTP head (no draft
    // file); Laguna uses a separate DFlash draft. "none" disables it.
    // UD-Q5_K_XL / UD-Q6 have no MTP layers — draft-max=4 aborts load (MTP context).
    let model_lc = cfg.model.to_ascii_lowercase();
    let no_mtp_quant = model_lc.contains("ud-q5") || model_lc.contains("ud-q6")
        || model_lc.contains("q5_k_xl") || model_lc.contains("q6_k_xl");
    let spec_type = if no_mtp_quant { "none".to_string() } else { cfg.spec_type.clone() };
    if spec_type != "none" && !spec_type.is_empty() {
        cmd.args(["--spec-type", &spec_type, "--spec-draft-n-max", "4"]);
        if !cfg.draft_model.is_empty() && file_size(&cfg.draft_model) > 0 {
            cmd.args(["--model-draft", &cfg.draft_model, "--spec-draft-ngl", "99"]);
        }
        // Grammar-forced drafts ride the same verify path (needs a draft path, so gated here).
        // Engine defaults on + adaptive-guarded; pass 0 only when the user disables it.
        cmd.args(["--spec-grammar-draft", if cfg.grammar_draft { "1" } else { "0" }]);
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
    Ok(annotate_auto_backend(
        &cfg.backend,
        "metal",
        "Server gestartet - lädt Modell (~60s)...".into(),
    ))
}

fn resource_dir_of(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    app.path().resource_dir().ok()
}

/// MLX via forked oMLX + PGRN when the launcher is present; else stock resident
/// `omlx serve`. Memory ceiling comes from Slipstream (`headroom_gb`).
fn start_mlx_server(
    app: &tauri::AppHandle,
    cfg: &ServerConfig,
    slot: &mut Option<Child>,
) -> Result<String, String> {
    let model_dir = {
        let trimmed = cfg.mlx_dir.trim();
        if trimmed.is_empty() {
            mlx::default_model_dir()
        } else {
            std::path::PathBuf::from(trimmed)
        }
    };
    let rd = resource_dir_of(app);
    let rd_ref = rd.as_deref();
    let streaming_launcher = mlx::pgrn_launcher(rd_ref).is_some();
    let has_sidecar = mlx::any_experts_sidecar(&model_dir);
    let need = mlx::largest_model_gib(&model_dir);
    let total_gib = sysctl_total_gib().unwrap_or(36.0);
    let free_gib = vm_stat_free_inactive_gib().unwrap_or(0.0);
    let ceiling = (total_gib - cfg.headroom_gb).max(1.0);
    if free_gib > 0.0 && free_gib < mlx::MLX_MIN_FREE_GIB {
        return Err(mlx::mlx_critical_free_refuse_msg(free_gib, mlx::MLX_MIN_FREE_GIB));
    }
    // Streaming: expert banks stay on SSD — do not refuse on full safetensors size.
    // Resident: refuse when the largest model cannot fit under the memory ceiling.
    if !(streaming_launcher && has_sidecar) {
        if need > 0.0 && need + 1.0 > ceiling {
            return Err(mlx::mlx_resident_refuse_msg(
                need,
                ceiling,
                cfg.headroom_gb,
                free_gib,
            ));
        }
    }
    // PGRN launcher uses Slipstream mlx-runtime (or legacy oMLX.app) — stock
    // `omlx` binary only needed for resident fallback without the launcher.
    let omlx = if streaming_launcher {
        std::path::PathBuf::from("omlx-unused")
    } else {
        mlx::resolve_omlx(&cfg.omlx_bin)?
    };
    let pgrn_env = mlx::PgrnMlxEnv::from_parts(
        &cfg.pgrn_profile,
        &cfg.pgrn_residency,
        cfg.pgrn_keep_hot,
        cfg.pgrn_warmup,
        cfg.pgrn_online,
        &cfg.pgrn_l3_peer_base,
        &cfg.mcp_config,
    );
    let (mut cmd, streaming) = mlx::serve_command(
        &omlx,
        &model_dir,
        cfg.port,
        ceiling,
        rd_ref,
        Some(&pgrn_env),
        cfg.memory_guard_off,
    )?;
    let log = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(LOG_PATH)
        .map_err(|e| format!("Log-Fehler: {e}"))?;
    let log_err = log.try_clone().map_err(|e| e.to_string())?;
    cmd.stdout(Stdio::from(log)).stderr(Stdio::from(log_err));
    if streaming && !mlx::python_runtime_available() {
        return Err(
            "MLX runtime missing — Settings → Install MLX runtime (one-time wheels), or install oMLX.app as fallback."
                .into(),
        );
    }
    let child = cmd.spawn().map_err(|e| format!("MLX start failed: {e}"))?;
    *slot = Some(child);
    let guard_note = if cfg.memory_guard_off {
        " · memory-guard off"
    } else {
        ""
    };
    let soft = mlx::mlx_low_free_soft_tip(free_gib, &pgrn_env.residency).unwrap_or_default();
    Ok(if streaming && has_sidecar {
        format!(
            "MLX+PGRN started (SSD expert streaming, ~{need:.0} GiB on disk; free ≈ {free:.0} GiB{guard}) — first chat loads…{soft}",
            free = free_gib,
            guard = guard_note,
            soft = soft,
        )
    } else if streaming {
        format!(
            "MLX started via PGRN launcher but experts.pgrn missing — resident fallback (~{need:.0} GiB, free ≈ {free:.0} GiB{guard}). Add experts.pgrn next to the model for SSD streaming.{soft}",
            free = free_gib,
            guard = guard_note,
            soft = soft,
        )
    } else {
        format!(
            "MLX server started (resident, ~{need:.0} GiB, free ≈ {free:.0} GiB{guard}) — first chat loads the model…{soft}",
            free = free_gib,
            guard = guard_note,
            soft = soft,
        )
    })
}

#[tauri::command]
fn mlx_capability(app: tauri::AppHandle, mlx_dir: String) -> mlx::MlxCapability {
    let dir = {
        let trimmed = mlx_dir.trim();
        if trimmed.is_empty() {
            mlx::default_model_dir()
        } else {
            std::path::PathBuf::from(trimmed)
        }
    };
    mlx::capability(resource_dir_of(&app).as_deref(), &dir)
}

#[tauri::command]
fn mlx_runtime_status(app: tauri::AppHandle) -> Result<String, String> {
    mlx::runtime_status_json(resource_dir_of(&app).as_deref())
}

#[tauri::command]
fn install_mlx_runtime(app: tauri::AppHandle) -> Result<String, String> {
    mlx::start_runtime_install(resource_dir_of(&app).as_deref())
}

fn sysctl_total_gib() -> Option<f64> {
    let out = Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    let bytes: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// free + inactive pages (Mach-visible), same basis as Settings memory panel.
fn vm_stat_free_inactive_gib() -> Option<f64> {
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
}

fn stop_server_impl(state: &AppState) {
    // Owned PID → lockfile holder → port-scoped llama → lsof listeners.
    // No broad `pkill -f omlx-server` (CRASH_AVOIDANCE).
    teardown::stop_server(
        &state.server,
        SERVER_PORT,
        port_alive,
        &teardown::default_lock_path(),
        &teardown::default_hands_off_path(),
    );
}

#[tauri::command]
fn stop_server(state: State<AppState>) -> Result<String, String> {
    stop_server_impl(&state);
    Ok("Server gestoppt.".into())
}

/// Last decode tok/s parsed from the server log tail (llama.cpp "eval time").
fn last_tps() -> Option<f64> {
    // Tail only — a multi-hour engine log must not be slurped on the menubar tick.
    let s = tray::log_tail(LOG_PATH, 256 * 1024)?;
    servestats::parse_last_tps(&s)
}

/// Compact menubar status line: state plus last decode speed when running.
fn tray_status_line() -> String {
    match http_status(&format!("http://127.0.0.1:{SERVER_PORT}/health")) {
        200 => match last_tps() {
            Some(v) => format!("Slipstream · {v:.0} tok/s"),
            None => "Slipstream · bereit".into(),
        },
        0 => "Slipstream · gestoppt".into(),
        _ => "Slipstream · lädt…".into(),
    }
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
    /// A `partition-weights.txt` sidecar next to the PGRN — the same file
    /// `start_server` auto-passes to the engine as `--pgrn-partition-weights`.
    weights: bool,
    /// An interrupted conversion that can be continued (R1.5).
    resume: ConvResume,
}

/// State of an interrupted conversion, read from the converter's journal
/// sidecar (`<pgrn>.partial.journal`). `records_*` count experts, so the UI can
/// say "angehalten bei 41 %" instead of just "da liegt eine Datei".
#[derive(Serialize, Default, Clone)]
struct ConvResume {
    resumable: bool,
    records_done: u64,
    records_total: u64,
    partial_bytes: u64,
    /// A `.partial` without a usable journal: only a fresh start can fix it.
    orphan_partial: bool,
}

const PGRNJ_MAGIC: &[u8; 8] = b"PGRNJRN1";
const PGRNJ_HEADER_BYTES: u64 = 120;
const PGRN_DIR_RECORD: u64 = 26;

fn conv_resume(pgrn: &str) -> ConvResume {
    let partial = format!("{pgrn}.partial");
    let partial_bytes = file_size(&partial);
    if partial_bytes == 0 {
        return ConvResume::default();
    }
    let journal = format!("{partial}.journal");
    let head = std::fs::read(&journal).unwrap_or_default();
    if head.len() < PGRNJ_HEADER_BYTES as usize || &head[..8] != PGRNJ_MAGIC {
        return ConvResume { orphan_partial: true, partial_bytes, ..Default::default() };
    }
    let u64_at = |off: usize| -> u64 {
        u64::from_le_bytes(head[off..off + 8].try_into().unwrap_or([0; 8]))
    };
    let records_done = (head.len() as u64 - PGRNJ_HEADER_BYTES) / PGRN_DIR_RECORD;
    ConvResume {
        resumable: true,
        records_done,
        records_total: u64_at(88).saturating_mul(u64_at(96)), // expert_count x layers
        partial_bytes,
        orphan_partial: false,
    }
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

/// Expand a first-shard path/URL ("…-00001-of-00006.gguf") into all N shard
/// strings by substituting the running index. Returns `[s]` for non-sharded
/// inputs. Works on any string (file path or URL) since only the tail differs.
fn shard_expand(s: &str) -> Vec<String> {
    // find the "-NNNNN-of-MMMMM.gguf" tail
    let Some(pos) = s.rfind("-of-") else { return vec![s.to_string()] };
    if !s.ends_with(".gguf") || pos < 5 {
        return vec![s.to_string()];
    }
    let no_str = &s[pos - 5..pos];
    let of_str = &s[pos + 4..s.len() - 5];
    let (Ok(_no), Ok(of)) = (no_str.parse::<u32>(), of_str.parse::<u32>()) else {
        return vec![s.to_string()];
    };
    if !(2..=999).contains(&of) {
        return vec![s.to_string()];
    }
    let prefix = &s[..pos - 5];
    (1..=of)
        .map(|i| format!("{prefix}{i:05}-of-{of:05}.gguf"))
        .collect()
}

/// Sum file sizes across all shards of a (possibly sharded) gguf path.
fn gguf_total_bytes(first: &str) -> u64 {
    shard_expand(first).iter().map(|p| file_size(p)).sum()
}

#[tauri::command]
fn model_status(gguf: String, pgrn: String, dir: String, state: State<AppState>) -> ModelStatus {
    ModelStatus {
        gguf_bytes: gguf_total_bytes(&gguf),
        pgrn_bytes: file_size(&pgrn),
        downloading: alive(&state.dl),
        converting: alive(&state.conv),
        disk_free_gib: disk_free_gib(&dir),
        weights: std::path::Path::new(&pgrn)
            .parent()
            .map(|d| d.join("partition-weights.txt").exists())
            .unwrap_or(false),
        resume: conv_resume(&pgrn),
    }
}

/// True when `path` exists and is a directory (UI path defaults / pickers).
#[tauri::command]
fn path_is_dir(path: String) -> bool {
    std::path::Path::new(&path).is_dir()
}

/// Mounted volumes that already have a `Modelle/` tree — candidates for the
/// optional second-disk GGUF base (`extBase` in the UI).
#[tauri::command]
fn list_ext_model_bases() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/Volumes") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let p = e.path().join("Modelle");
        if p.is_dir() {
            out.push(p.to_string_lossy().into_owned());
        }
    }
    out.sort();
    out
}

/// Content-Length of the (redirect-followed) URL, summed across all shards for
/// a sharded first-shard URL, or 0.
#[tauri::command]
fn remote_size(url: String) -> u64 {
    let one = |u: &str| -> u64 {
        (|| {
            let out = Command::new("curl").args(["-sIL", u]).output().ok()?;
            let s = String::from_utf8_lossy(&out.stdout);
            s.lines()
                .filter(|l| l.to_lowercase().starts_with("content-length:"))
                .filter_map(|l| l.split(':').nth(1)?.trim().parse::<u64>().ok())
                .max()
        })()
        .unwrap_or(0)
    };
    shard_expand(&url).iter().map(|u| one(u)).sum()
}

#[tauri::command]
fn start_download(url: String, dest: String, dir: String, state: State<AppState>) -> Result<String, String> {
    if alive(&state.dl) {
        return Err("Download läuft bereits.".into());
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("Ordner: {e}"))?;
    let log = OpenOptions::new().create(true).write(true).truncate(true).open(DL_LOG)
        .map_err(|e| e.to_string())?;
    // Sharded XL models: one curl per shard, sequential, each resumable (-C -).
    // The shard index in dest and URL advance in lockstep (shard_expand).
    let dests = shard_expand(&dest);
    let urls = shard_expand(&url);
    let child = if dests.len() > 1 && dests.len() == urls.len() {
        let mut script = String::from("set -e\n");
        for (d, u) in dests.iter().zip(urls.iter()) {
            script += &format!(
                "curl -L -C - --fail --retry 5 -o {} {}\n",
                shell_quote(d), shell_quote(u)
            );
        }
        script += "echo ALL_SHARDS_DONE\n";
        Command::new("sh").arg("-c").arg(script)
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
    } else {
        Command::new("curl")
            .args(["-L", "-C", "-", "--fail", "--retry", "5", "-o", &dest, &url])
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
    }
    .map_err(|e| format!("curl-Start: {e}"))?;
    *state.dl.lock().map_err(|e| e.to_string())? = Some(child);
    Ok("Download gestartet.".into())
}

/// Minimal POSIX single-quote escaping for a shell argument.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[tauri::command]
fn cancel_download(state: State<AppState>) {
    kill(&state.dl);
}

/// Resolve the bundled native GGUF->PGRN converter (Reloaded R1). Prefers the
/// self-contained binary inside the .app; falls back to the dev build dir.
fn convert_bin(app: &tauri::AppHandle) -> String {
    app.path()
        .resource_dir()
        .ok()
        .and_then(|d| {
            ["resources/pgrn-convert", "pgrn-convert"]
                .iter()
                .map(|c| d.join(c))
                .find(|p| p.exists())
        })
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            let repo = concat!(env!("CARGO_MANIFEST_DIR"), "/../../..");
            let repo = std::fs::canonicalize(repo)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            format!("{repo}/vendor/llama.cpp/build-static/bin/llama-pgrn-convert")
        })
}

#[tauri::command]
fn start_convert(app: tauri::AppHandle, gguf: String, pgrn: String, io_threads: u32, resume: bool, state: State<AppState>) -> Result<String, String> {
    if alive(&state.conv) {
        return Err("Konvertierung läuft bereits.".into());
    }
    if file_size(&gguf) == 0 {
        return Err("GGUF fehlt - erst herunterladen.".into());
    }
    if file_size(&pgrn) > 0 {
        return Err("PGRN existiert bereits - erst löschen, dann neu konvertieren.".into());
    }
    let partial = format!("{pgrn}.partial");
    let journal = format!("{partial}.journal");
    let state_before = conv_resume(&pgrn);
    if resume && !state_before.resumable {
        return Err("Keine fortsetzbare Konvertierung gefunden - bitte neu starten.".into());
    }
    if !resume {
        // Starting over: drop the interrupted work explicitly. Safe here because
        // no conversion we own is alive.
        let _ = std::fs::remove_file(&partial);
        let _ = std::fs::remove_file(&journal);
    }
    let bin = convert_bin(&app);
    if file_size(&bin) == 0 {
        return Err("Converter-Binary fehlt (pgrn-convert nicht gebündelt?).".into());
    }
    let threads = io_threads.clamp(1, 16).to_string();
    // Resuming appends to the log so the earlier attempt stays readable.
    let log = OpenOptions::new().create(true).write(true).append(resume).truncate(!resume)
        .open(CONV_LOG)
        .map_err(|e| e.to_string())?;
    let mut args = vec![
        "--input", &gguf,
        "--output", &pgrn,
        "--min-free-gb", "8",
        "--io-threads", &threads,
        "--progress", "jsonl",
    ];
    if resume {
        args.push("--resume");
    }
    let child = Command::new(&bin)
        .args(&args)
        .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|e| format!("Converter-Start: {e}"))?;
    *state.conv.lock().map_err(|e| e.to_string())? = Some(child);
    Ok(if resume {
        format!("Konvertierung fortgesetzt ({} von {} Experten bereits fertig).",
                state_before.records_done, state_before.records_total)
    } else {
        "Konvertierung gestartet.".into()
    })
}

/// Discard an interrupted conversion's `.partial` + journal (UI: "Neu beginnen").
#[tauri::command]
fn discard_convert(pgrn: String, state: State<AppState>) -> Result<String, String> {
    if alive(&state.conv) {
        return Err("Konvertierung läuft - erst abbrechen.".into());
    }
    let partial = format!("{pgrn}.partial");
    let _ = std::fs::remove_file(format!("{partial}.journal"));
    let _ = std::fs::remove_file(&partial);
    Ok("Angehaltene Konvertierung verworfen.".into())
}

/// Last JSONL progress line the converter wrote (phase/done/total/mb_s/eta).
/// The UI polls this while `model_status.converting` to render a real bar.
#[derive(Serialize, Default)]
struct ConvProgress {
    phase: String,
    done_bytes: u64,
    total_bytes: u64,
    mb_s: f64,
    eta_s: f64,
    expert: u64,
    expert_total: u64,
    message: String,
    /// Set on the converter's final `cancelled` line (R1.5).
    resumable: bool,
    records_done: u64,
    records_total: u64,
}

#[tauri::command]
fn convert_progress() -> ConvProgress {
    let s = std::fs::read_to_string(CONV_LOG).unwrap_or_default();
    let line = s
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or("");
    let v: Value = serde_json::from_str(line).unwrap_or(Value::Null);
    ConvProgress {
        phase: v["phase"].as_str().unwrap_or("").into(),
        done_bytes: v["done_bytes"].as_u64().unwrap_or(0),
        total_bytes: v["total_bytes"].as_u64().unwrap_or(0),
        mb_s: v["mb_s"].as_f64().unwrap_or(0.0),
        eta_s: v["eta_s"].as_f64().unwrap_or(0.0),
        expert: v["expert"].as_u64().unwrap_or(0),
        expert_total: v["expert_total"].as_u64().unwrap_or(0),
        message: v["message"].as_str().unwrap_or("").into(),
        resumable: v["resumable"].as_bool().unwrap_or(false),
        records_done: v["records_done"].as_u64().unwrap_or(0),
        records_total: v["records_total"].as_u64().unwrap_or(0),
    }
}

#[tauri::command]
fn cancel_convert(state: State<AppState>) {
    // SIGTERM first: the converter traps it, flushes its journal, and keeps the
    // .partial so `start_convert(resume = true)` can continue it. Hard-kill only
    // if it doesn't exit within ~2s — the journal still describes a valid prefix
    // then, because it is only ever fsynced behind durable payload.
    if let Ok(mut g) = state.conv.lock() {
        if let Some(mut c) = g.take() {
            let _ = Command::new("kill").args(["-TERM", &c.id().to_string()]).status();
            for _ in 0..20 {
                if matches!(c.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[tauri::command]
fn tail_file(path: String, max_lines: usize) -> String {
    let s = std::fs::read_to_string(&path).unwrap_or_default();
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

/// MIME for chat attach data URLs (images → `image_url`; docs → oMLX `file` parts).
/// Matches oMLX MarkItDown attachment extensions (+ common image types).
fn chat_attach_mime(ext: &str) -> Option<&'static str> {
    match ext {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "pdf" => Some("application/pdf"),
        "txt" => Some("text/plain"),
        "md" | "markdown" => Some("text/markdown"),
        "docx" => Some(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        ),
        "pptx" => Some(
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        ),
        _ => None,
    }
}

/// Read a local image or document as a `data:<mime>;base64,…` URL for
/// OpenAI-compatible chat parts (`image_url` / oMLX MarkItDown `file`).
#[tauri::command]
fn read_file_data_url(path: String) -> Result<String, String> {
    // Align with oMLX `markitdown_max_file_size_mb` default (25).
    const MAX_BYTES: u64 = 25 * 1024 * 1024;
    let p = std::path::Path::new(path.trim());
    if !p.is_file() {
        return Err(format!("Datei fehlt: {}", p.display()));
    }
    let meta = std::fs::metadata(p).map_err(|e| e.to_string())?;
    if meta.len() > MAX_BYTES {
        return Err(format!(
            "Datei zu groß ({:.1} MiB) — max. {} MiB.",
            meta.len() as f64 / (1024.0 * 1024.0),
            MAX_BYTES / (1024 * 1024)
        ));
    }
    let bytes = std::fs::read(p).map_err(|e| format!("Lesen: {e}"))?;
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mime = chat_attach_mime(&ext).ok_or_else(|| {
        format!(
            "Nicht unterstützter Typ: .{ext} (Bilder: png/jpg/gif/webp/bmp; \
             Docs: pdf/md/txt/docx/pptx)"
        )
    })?;
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{mime};base64,{b64}"))
}

#[cfg(test)]
mod chat_attach_mime_tests {
    use super::chat_attach_mime;

    #[test]
    fn images_and_docs() {
        assert_eq!(chat_attach_mime("png"), Some("image/png"));
        assert_eq!(chat_attach_mime("pdf"), Some("application/pdf"));
        assert_eq!(chat_attach_mime("md"), Some("text/markdown"));
        assert_eq!(chat_attach_mime("txt"), Some("text/plain"));
        assert!(chat_attach_mime("docx").unwrap().contains("wordprocessingml"));
        assert!(chat_attach_mime("pptx").unwrap().contains("presentationml"));
        assert_eq!(chat_attach_mime("xlsx"), None);
        assert_eq!(chat_attach_mime("exe"), None);
    }

    #[test]
    fn image_aliases_and_extra_docs() {
        assert_eq!(chat_attach_mime("jpg"), Some("image/jpeg"));
        assert_eq!(chat_attach_mime("jpeg"), Some("image/jpeg"));
        assert_eq!(chat_attach_mime("gif"), Some("image/gif"));
        assert_eq!(chat_attach_mime("webp"), Some("image/webp"));
        assert_eq!(chat_attach_mime("bmp"), Some("image/bmp"));
        assert_eq!(chat_attach_mime("markdown"), Some("text/markdown"));
        assert_eq!(chat_attach_mime(""), None);
        assert_eq!(chat_attach_mime("PNG"), None); // caller lowercases; map is lowercase-only
    }

    #[test]
    fn rejects_unsupported_office_web_and_archives() {
        for ext in ["html", "htm", "svg", "zip", "xlsx", "csv", "json", "rtf"] {
            assert_eq!(chat_attach_mime(ext), None, "ext={ext}");
        }
    }
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

/// Show + focus the main window (used by the tray "open" item and left-click).
fn show_main(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .manage(p2p::P2pState::default())
        .invoke_handler(tauri::generate_handler![
            start_server, stop_server, is_running, server_state, read_log, system_stats,
            model_status, path_is_dir, list_ext_model_bases, remote_size, start_download, cancel_download,
            start_convert, cancel_convert, discard_convert, convert_progress, tail_file, patch_kilo_config, defaults,
            start_embedder, stop_embedder, install_qdrant, start_qdrant, stop_qdrant, index_status,
            live_stats, clear_stats, runtime_preflight, inspect_storage, mlx_capability, mlx_runtime_status, install_mlx_runtime,
            read_file_data_url,
            // Slipstream P2P (UI gate: localStorage `slipstream.p2p`; Cluster tab)
            p2p::p2p_status, p2p::p2p_start, p2p::p2p_stop, p2p::p2p_send_test_job,
            p2p::p2p_chat, p2p::p2p_peers, p2p::p2p_recent_peers, p2p::p2p_credits
        ])
        .setup(|app| {
            app.manage(Live {
                serving: Mutex::new(servestats::Store::load()),
                system: Mutex::new(sysstats::SysSnapshot::default()),
                api_extras: Mutex::new(servestats::ApiExtras::default()),
            });
            tray::install(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // Close = hide to tray (menubar app stays resident), not quit.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running Peregrine Control");
}
