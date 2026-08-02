//! MLX path via forked oMLX (PGRN SSD staging) or Homebrew / oMLX.app resident.
//!
//! Prefer the Slipstream-bundled launcher (`resources/omlx-pgrn/`) when present
//! so `/Applications/Slipstream.app` can start SSD expert streaming without a
//! checkout. Python/MLX wheels come from:
//!   1. `~/Library/Application Support/Slipstream/mlx-runtime` (bootstrap)
//!   2. `/Applications/oMLX.app` (legacy fallback)
//! Falls back to the repo `tools/pgrn-mlx` layout for `cargo run`,
//! then to stock `omlx` (fully resident).

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Parent directory that contains one subdirectory per MLX model (config.json +
/// safetensors). Matches `omlx serve --model-dir`.
/// Canonical: `~/Modelle/mlx` on internal SSD. External volumes are Advanced overflow only.
pub fn default_model_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Modelle/mlx");
    }
    PathBuf::from("/tmp/Modelle/mlx")
}

/// Application-support root so Slipstream does not share `~/.omlx` with a
/// separately installed oMLX.
pub fn base_path() -> PathBuf {
    dirs_next_data().join("omlx-pgrn")
}

fn dirs_next_data() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Library/Application Support/Slipstream");
    }
    PathBuf::from("/tmp/slipstream")
}

/// `~/Library/Application Support/Slipstream/mlx-runtime` — CPython + MLX wheels.
pub fn mlx_runtime_root() -> PathBuf {
    if let Ok(p) = std::env::var("SLIPSTREAM_MLX_RUNTIME") {
        let trimmed = p.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    dirs_next_data().join("mlx-runtime")
}

/// True when bootstrap finished (`READY` + python).
/// Canonical interpreter: `venv/bin/python` (see `bootstrap_mlx_runtime.sh`).
/// Also accepts a flat `bin/python{,3}` layout if present.
pub fn mlx_runtime_ready() -> bool {
    let root = mlx_runtime_root();
    root.join("READY").is_file()
        && (root.join("venv/bin/python").is_file()
            || root.join("bin/python").is_file()
            || root.join("bin/python3").is_file())
}

pub fn omlx_app_present() -> bool {
    Path::new("/Applications/oMLX.app/Contents/Resources/Python/cpython-3.11/bin/python3")
        .is_file()
}

/// Either Application Support mlx-runtime or legacy oMLX.app can run the launcher.
pub fn python_runtime_available() -> bool {
    mlx_runtime_ready() || omlx_app_present()
}

/// Resolve `omlx`: explicit override, then PATH, then Homebrew, then oMLX.app CLI.
pub fn resolve_omlx(override_path: &str) -> Result<PathBuf, String> {
    let trimmed = override_path.trim();
    if !trimmed.is_empty() {
        let p = PathBuf::from(trimmed);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!("omlx binary not found at {trimmed}"));
    }
    if let Ok(path) = which("omlx") {
        return Ok(path);
    }
    for brew in ["/opt/homebrew/bin/omlx", "/usr/local/bin/omlx"] {
        let p = PathBuf::from(brew);
        if p.is_file() {
            return Ok(p);
        }
    }
    Err(
        "omlx not found. Install the Slipstream MLX runtime (Settings), oMLX.app, or `brew install omlx`."
            .into(),
    )
}

fn which(name: &str) -> Result<PathBuf, ()> {
    let out = Command::new("/usr/bin/which")
        .arg(name)
        .output()
        .map_err(|_| ())?;
    if !out.status.success() {
        return Err(());
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let path = PathBuf::from(line.trim());
    if path.is_file() {
        Ok(path)
    } else {
        Err(())
    }
}

/// True if `dir` is a usable `--model-dir`: at least one child with config.json.
pub fn model_dir_ready(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let p = entry.path();
        p.is_dir() && p.join("config.json").is_file()
    })
}

/// Estimated resident GiB from the largest model subdirectory (file sizes).
pub fn largest_model_gib(dir: &Path) -> f64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0.0;
    };
    let mut best = 0u64;
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let size = dir_bytes(&p);
        if size > best {
            best = size;
        }
    }
    best as f64 / (1024.0 * 1024.0 * 1024.0)
}

fn dir_bytes(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
        .sum()
}

/// True when a model subdirectory under `model_dir` has `experts.pgrn`.
pub fn any_experts_sidecar(model_dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(model_dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let p = entry.path();
        p.is_dir()
            && (p.join("experts.pgrn").is_file()
                || p
                    .parent()
                    .map(|parent| {
                        parent.join(format!(
                            "{}.pgrn",
                            p.file_name().unwrap_or_default().to_string_lossy()
                        ))
                    })
                    .map(|alt| alt.is_file())
                    .unwrap_or(false))
    })
}

/// Prompt-char threshold for Auto (hybrid): at/above → prefer Metal (long prefill
/// amortizes SSD fetch; measured ~2.7× on large agent prompts). Below + `experts.pgrn`
/// → prefer MLX (short/warm decode ~Metal-class after residency gate).
pub const AUTO_PREFILL_CHARS: usize = 8000;

/// Resolve Settings backend to an effective engine. Explicit `metal`/`mlx` are sticky.
/// `auto` / `heuristic`: long prefill → metal; short/warm → mlx only when experts.pgrn exists.
pub fn resolve_backend(preference: &str, prompt_chars: usize, has_experts_pgrn: bool) -> &'static str {
    match preference {
        "mlx" => "mlx",
        "metal" => "metal",
        "auto" | "heuristic" => {
            if prompt_chars >= AUTO_PREFILL_CHARS {
                "metal"
            } else if has_experts_pgrn {
                "mlx"
            } else {
                "metal"
            }
        }
        _ => "metal",
    }
}

/// Count model subdirs and how many have an experts.pgrn sidecar.
pub fn model_sidecar_counts(model_dir: &Path) -> (usize, usize) {
    let Ok(entries) = std::fs::read_dir(model_dir) else {
        return (0, 0);
    };
    let mut models = 0usize;
    let mut with_pgrn = 0usize;
    for entry in entries.flatten() {
        let p = entry.path();
        if !(p.is_dir() && p.join("config.json").is_file()) {
            continue;
        }
        models += 1;
        if p.join("experts.pgrn").is_file() {
            with_pgrn += 1;
        }
    }
    (models, with_pgrn)
}

fn launcher_is_ready(script: &Path) -> bool {
    if !script.is_file() {
        return false;
    }
    let Some(dir) = script.parent() else {
        return false;
    };
    // Bundled layout
    if dir.join("libpgrn_host.dylib").is_file() && dir.join("omlx/pgrn").is_dir() {
        return true;
    }
    // Dev checkout layout
    if dir.join("native/build/libpgrn_host.dylib").is_file() {
        return true;
    }
    false
}

/// Directory that holds the staged/bundled oMLX+PGRN sidecar, if any.
pub fn pgrn_bundle_dir(resource_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(rd) = resource_dir {
        for rel in ["resources/omlx-pgrn", "omlx-pgrn"] {
            let d = rd.join(rel);
            if d.join("run_omlx_pgrn.sh").is_file() && launcher_is_ready(&d.join("run_omlx_pgrn.sh"))
            {
                return Some(d);
            }
        }
    }
    None
}

/// Launcher that puts the Slipstream oMLX fork on PYTHONPATH and sets
/// `SLIPSTREAM_PGRN*`. No hardcoded absolute checkout paths.
///
/// Order: env override → app resources → repo-relative (cargo / sibling of
/// resources folder in a checkout).
pub fn pgrn_launcher(resource_dir: Option<&Path>) -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SLIPSTREAM_OMLX_LAUNCHER") {
        let path = PathBuf::from(p.trim());
        if launcher_is_ready(&path) {
            return Some(path);
        }
    }
    if let Some(dir) = pgrn_bundle_dir(resource_dir) {
        return Some(dir.join("run_omlx_pgrn.sh"));
    }
    // Canonical app checkout first, then the legacy monorepo tools layout.
    for checkout in [
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("resources/omlx-pgrn/run_omlx_pgrn.sh"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../tools/pgrn-mlx/run_omlx_pgrn.sh"),
    ] {
        if let Ok(c) = std::fs::canonicalize(&checkout) {
            if launcher_is_ready(&c) {
                return Some(c);
            }
        } else if launcher_is_ready(&checkout) {
            return Some(checkout);
        }
    }
    None
}

/// `bootstrap_mlx_runtime.sh` next to the launcher (bundle or checkout).
pub fn bootstrap_script(resource_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = pgrn_bundle_dir(resource_dir) {
        let s = dir.join("bootstrap_mlx_runtime.sh");
        if s.is_file() {
            return Some(s);
        }
    }
    if let Some(launcher) = pgrn_launcher(resource_dir) {
        if let Some(parent) = launcher.parent() {
            let s = parent.join("bootstrap_mlx_runtime.sh");
            if s.is_file() {
                return Some(s);
            }
        }
    }
    let checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../tools/pgrn-mlx/bootstrap_mlx_runtime.sh");
    if checkout.is_file() {
        return Some(checkout);
    }
    None
}

#[derive(Debug, Clone, Serialize)]
pub struct MlxCapability {
    /// "streaming" | "resident" | "unavailable"
    pub mode: String,
    pub omlx_app: bool,
    /// Application Support mlx-runtime bootstrap complete.
    pub runtime_ready: bool,
    /// ready | missing | installing | failed | unknown
    pub runtime_state: String,
    /// runtime | omlx_app | none
    pub provider: String,
    pub launcher: bool,
    pub lib_bundled: bool,
    pub fork_bundled: bool,
    pub model_dir_ok: bool,
    pub models: usize,
    pub models_with_experts_pgrn: usize,
    pub detail: String,
}

fn read_runtime_state_file() -> Option<(String, String)> {
    let path = mlx_runtime_root().join("status.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return None;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return None;
    };
    let state = v
        .get("state")
        .and_then(|x| x.as_str())
        .unwrap_or("unknown")
        .to_string();
    let detail = v
        .get("detail")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    Some((state, detail))
}

/// Snapshot for Settings UI: what the MLX backend will do on Start.
pub fn capability(resource_dir: Option<&Path>, mlx_dir: &Path) -> MlxCapability {
    let omlx_app = omlx_app_present();
    let runtime_ready = mlx_runtime_ready();
    let python_ok = runtime_ready || omlx_app;
    let provider = if runtime_ready {
        "runtime"
    } else if omlx_app {
        "omlx_app"
    } else {
        "none"
    };
    let (file_state, file_detail) = read_runtime_state_file().unwrap_or_else(|| {
        if runtime_ready {
            ("ready".into(), String::new())
        } else {
            ("missing".into(), String::new())
        }
    });
    let runtime_state = if runtime_ready {
        "ready".into()
    } else {
        file_state
    };

    let launcher_path = pgrn_launcher(resource_dir);
    let launcher = launcher_path.is_some();
    let bundle = pgrn_bundle_dir(resource_dir);
    let lib_bundled = bundle
        .as_ref()
        .map(|d| d.join("libpgrn_host.dylib").is_file())
        .unwrap_or(false)
        || launcher_path
            .as_ref()
            .and_then(|p| {
                p.parent().map(|d| {
                    d.join("native/build/libpgrn_host.dylib").is_file()
                        || d.join("libpgrn_host.dylib").is_file()
                })
            })
            .unwrap_or(false);
    let fork_bundled = bundle
        .as_ref()
        .map(|d| d.join("omlx/pgrn").is_dir())
        .unwrap_or(false)
        || PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../vendor/omlx/omlx/pgrn")
            .is_dir();

    let model_dir_ok = model_dir_ready(mlx_dir);
    let (models, with_pgrn) = model_sidecar_counts(mlx_dir);
    let stock_omlx = resolve_omlx("").is_ok();

    let (mode, detail) = if launcher && !python_ok {
        let hint = if runtime_state == "installing" {
            if file_detail.is_empty() {
                "MLX runtime installing — wait, then Start.".into()
            } else {
                file_detail
            }
        } else {
            "MLX runtime not installed — one-time download of MLX wheels (~0.5–1 GiB). Click Install MLX runtime (oMLX.app optional fallback)."
                .into()
        };
        ("unavailable".into(), hint)
    } else if !launcher && !python_ok && !stock_omlx {
        (
            "unavailable".into(),
            "Install MLX runtime (Settings) or oMLX.app — required for the MLX backend.".into(),
        )
    } else if launcher && with_pgrn > 0 {
        let via = if runtime_ready {
            "Slipstream mlx-runtime"
        } else {
            "oMLX.app"
        };
        (
            "streaming".into(),
            format!(
                "MLX + SSD PGRN ready via {via} — {with_pgrn}/{models} model(s) have experts.pgrn (profile balanced)."
            ),
        )
    } else if launcher && models > 0 {
        (
            "resident".into(),
            format!(
                "Launcher present but experts.pgrn missing for {models} model(s) — Start falls back to resident (no expert streaming)."
            ),
        )
    } else if launcher {
        (
            "streaming".into(),
            "MLX + PGRN launcher ready — set MLX model dir; place experts.pgrn next to each model for SSD streaming.".into(),
        )
    } else if omlx_app || stock_omlx {
        (
            "resident".into(),
            "Stock oMLX only (PGRN sidecar not bundled) — fully resident, no SSD expert streaming.".into(),
        )
    } else {
        ("unavailable".into(), "MLX backend unavailable.".into())
    };

    MlxCapability {
        mode,
        omlx_app,
        runtime_ready,
        runtime_state,
        provider: provider.into(),
        launcher,
        lib_bundled,
        fork_bundled,
        model_dir_ok,
        models,
        models_with_experts_pgrn: with_pgrn,
        detail,
    }
}

/// Run `bootstrap_mlx_runtime.sh --json` (status only; does not install).
pub fn runtime_status_json(resource_dir: Option<&Path>) -> Result<String, String> {
    let script = bootstrap_script(resource_dir)
        .ok_or("bootstrap_mlx_runtime.sh not found (stage resources/omlx-pgrn)")?;
    let out = Command::new("/bin/bash")
        .arg(&script)
        .arg("--json")
        .env_remove("PYTHONHOME")
        .env_remove("PYTHONPATH")
        .output()
        .map_err(|e| format!("bootstrap --json failed: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("bootstrap --json exit {}: {err}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Start one-time MLX wheel bootstrap in the background (idempotent).
pub fn start_runtime_install(resource_dir: Option<&Path>) -> Result<String, String> {
    if mlx_runtime_ready() {
        return Ok("MLX runtime already ready.".into());
    }
    let script = bootstrap_script(resource_dir)
        .ok_or("bootstrap_mlx_runtime.sh not found (stage resources/omlx-pgrn)")?;
    let log = mlx_runtime_root().join("bootstrap.log");
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|e| format!("bootstrap log: {e}"))?;
    let log_err = log_file.try_clone().map_err(|e| e.to_string())?;
    Command::new("/bin/bash")
        .arg(&script)
        .arg("install")
        .env_remove("PYTHONHOME")
        .env_remove("PYTHONPATH")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err))
        .spawn()
        .map_err(|e| format!("failed to start MLX runtime install: {e}"))?;
    Ok("MLX runtime install started (one-time wheel download).".into())
}

/// Settings-backed `SLIPSTREAM_PGRN_*` (+ optional oMLX MCP) env for MLX serve.
/// Metal ignores these.
///
/// Interactive product defaults: profile io=16, no cold-io boost, residency=
/// `touch` (mlock opt-in), keep-hot + warmup on. Benches pin `RESIDENCY=mlock`
/// for the recovered ~18.9 tok/s io16 recipe — see PERF_RECOVERY.md /
/// CRASH_AVOIDANCE.md. Prefetch stays opt-in via process env only.
/// MCP is default OFF — empty `mcp_config` leaves `OMLX_MCP_CONFIG` unset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgrnMlxEnv {
    pub profile: String,
    pub residency: String,
    pub keep_hot: bool,
    pub warmup: bool,
    /// Settings → Advanced predictive prefetch → `SLIPSTREAM_PGRN_ONLINE` (default OFF).
    pub online: bool,
    /// When non-empty → `SLIPSTREAM_PGRN_L3=peer` + `PEER_BASE` (default OFF).
    pub l3_peer_base: String,
    /// When non-empty → `OMLX_MCP_CONFIG` + `--mcp-config` (default OFF).
    pub mcp_config: String,
}

impl Default for PgrnMlxEnv {
    fn default() -> Self {
        Self {
            profile: "balanced".into(),
            residency: "touch".into(),
            keep_hot: true,
            warmup: true,
            online: false,
            l3_peer_base: String::new(),
            mcp_config: String::new(),
        }
    }
}

impl PgrnMlxEnv {
    pub fn from_parts(
        profile: impl Into<String>,
        residency: impl Into<String>,
        keep_hot: bool,
        warmup: bool,
        online: bool,
        l3_peer_base: impl Into<String>,
        mcp_config: impl Into<String>,
    ) -> Self {
        Self {
            profile: profile.into(),
            residency: residency.into(),
            keep_hot,
            warmup,
            online,
            l3_peer_base: l3_peer_base.into(),
            mcp_config: mcp_config.into(),
        }
        .sanitized()
    }

    pub fn sanitized(mut self) -> Self {
        let p = self.profile.trim().to_ascii_lowercase();
        self.profile = match p.as_str() {
            "quality" | "fast" | "balanced" | "contract" => p,
            _ => "balanced".into(),
        };
        let r = self.residency.trim().to_ascii_lowercase();
        self.residency = match r.as_str() {
            "mlock" | "1" | "true" | "yes" => "mlock".into(),
            "touch" => "touch".into(),
            "off" | "0" | "false" | "no" => "off".into(),
            // Unrecognized → interactive-safe default (mlock is opt-in).
            _ => "touch".into(),
        };
        self.l3_peer_base = self.l3_peer_base.trim().to_string();
        self.mcp_config = self.mcp_config.trim().to_string();
        self
    }

    /// Child env pairs. Empty peer base / mcp_config leave those features OFF.
    pub fn env_pairs(&self) -> Vec<(String, String)> {
        let s = self.clone().sanitized();
        let mut out = vec![
            ("SLIPSTREAM_PGRN_PROFILE".into(), s.profile),
            ("SLIPSTREAM_PGRN_RESIDENCY".into(), s.residency),
            (
                "SLIPSTREAM_PGRN_KEEP_HOT".into(),
                if s.keep_hot { "1" } else { "0" }.into(),
            ),
            (
                "SLIPSTREAM_PGRN_WARMUP".into(),
                if s.warmup { "1" } else { "0" }.into(),
            ),
            // Pin recovered recipe: balanced/quality profile io=16, no cold boost.
            ("SLIPSTREAM_PGRN_COLD_IO_WIDTH".into(), "0".into()),
            (
                "SLIPSTREAM_PGRN_ONLINE".into(),
                if s.online { "1" } else { "0" }.into(),
            ),
        ];
        if !s.l3_peer_base.is_empty() {
            out.push(("SLIPSTREAM_PGRN_L3".into(), "peer".into()));
            out.push(("SLIPSTREAM_PGRN_PEER_BASE".into(), s.l3_peer_base));
        }
        if !s.mcp_config.is_empty() {
            out.push(("OMLX_MCP_CONFIG".into(), s.mcp_config));
        }
        out
    }

    pub fn apply_to(&self, cmd: &mut Command) {
        for (k, v) in self.env_pairs() {
            cmd.env(k, v);
        }
    }
}

/// oMLX `--mcp-config` CLI args. Empty path → no args (MCP stays OFF).
pub fn mcp_cli_args(mcp_config: &str) -> Vec<String> {
    let p = mcp_config.trim();
    if p.is_empty() {
        vec![]
    } else {
        vec!["--mcp-config".into(), p.to_string()]
    }
}

/// True when Settings MCP path looks usable (absolute POSIX/Windows path).
/// Empty is valid (MCP OFF). Relative paths are rejected so serve doesn't
/// silently resolve against cwd.
pub fn mcp_config_path_ok(mcp_config: &str) -> bool {
    let p = mcp_config.trim();
    if p.is_empty() {
        return true;
    }
    if p.starts_with('/') {
        return true;
    }
    // Windows: C:\… or C:/…
    let bytes = p.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

/// oMLX memory-guard CLI args.
///
/// Default product path: `--memory-guard-gb` = total − headroom.
/// Opt-in Settings escape for Metal wired ~28 GiB: `--memory-guard off`
/// (see `prefill_memory_exceeded` / CODING_SMOKE.md).
pub fn memory_guard_cli_args(memory_guard_gb: f64, off: bool) -> Vec<String> {
    if off {
        vec!["--memory-guard".into(), "off".into()]
    } else {
        vec![
            "--memory-guard-gb".into(),
            format!("{memory_guard_gb:.1}"),
        ]
    }
}

/// Clear refuse text when resident MLX cannot fit under the Slipstream ceiling.
pub fn mlx_resident_refuse_msg(
    need_gib: f64,
    ceiling_gib: f64,
    headroom_gib: f64,
    free_gib: f64,
) -> String {
    format!(
        "RAM too low for resident MLX: model needs ~{need:.1} GiB, \
ceiling after {headroom:.1} GiB reserve is {ceiling:.1} GiB \
(free+inactive ≈ {free:.1} GiB). Close apps, lower the reserve in Settings, \
or add experts.pgrn next to the model for SSD streaming. \
Tip: long coding/full-PGRN can also hit the Metal wired cap (~28 GiB) — \
enable “Memory guard off” in Settings or raise iogpu.wired_limit_mb (sudo).",
        need = need_gib,
        headroom = headroom_gib,
        ceiling = ceiling_gib,
        free = free_gib,
    )
}

/// Clear refuse when free+inactive is critically low before any MLX serve.
pub fn mlx_critical_free_refuse_msg(free_gib: f64, min_free_gib: f64) -> String {
    format!(
        "RAM too low to start MLX safely: free+inactive ≈ {free:.1} GiB \
(need ≥ {min:.1} GiB). Close browsers/IDEs and retry. \
Metal wired cap is ~28 GiB — even with headroom, long prefill can fail \
with prefill_memory_exceeded unless you enable “Memory guard off” in Settings.",
        free = free_gib,
        min = min_free_gib,
    )
}

/// Floor free+inactive before spawning MLX (product start, not bench).
pub const MLX_MIN_FREE_GIB: f64 = 4.0;

/// Soft-warn band (matches safety watchdog MEMORY floor). Start still allowed
/// when free ≥ `MLX_MIN_FREE_GIB`; UI toasts + success tip nudge the user.
pub const MLX_WARN_FREE_GIB: f64 = 8.0;

/// Optional tip appended to a successful MLX start when free RAM is in the
/// warn band `[MLX_MIN_FREE_GIB, MLX_WARN_FREE_GIB)`. Does not refuse —
/// that is `MLX_MIN_FREE_GIB`. Below the critical floor returns `None`
/// (refuse path owns that UX; never imply a successful start tip).
pub fn mlx_low_free_soft_tip(free_gib: f64, residency: &str) -> Option<String> {
    if !(free_gib >= MLX_MIN_FREE_GIB && free_gib < MLX_WARN_FREE_GIB) {
        return None;
    }
    let residency_note = if residency.eq_ignore_ascii_case("mlock") {
        " residency=mlock is risky here — prefer touch for interactive use."
    } else {
        ""
    };
    Some(format!(
        " Tip: free+inactive ≈ {free:.1} GiB is tight (< {warn:.0} GiB) — \
close apps before long coding; avoid overnight mlock.{extra}",
        free = free_gib,
        warn = MLX_WARN_FREE_GIB,
        extra = residency_note,
    ))
}

/// Build the serve command. Prefers the PGRN fork launcher; else stock omlx.
///
/// `pgrn_env` comes from Settings (or defaults). Applied unconditionally so the
/// UI wins over ambient process env for these keys.
/// `memory_guard_off`: Settings opt-in → `--memory-guard off` instead of gb.
pub fn serve_command(
    omlx: &Path,
    model_dir: &Path,
    port: u16,
    memory_guard_gb: f64,
    resource_dir: Option<&Path>,
    pgrn_env: Option<&PgrnMlxEnv>,
    memory_guard_off: bool,
) -> Result<(Command, bool), String> {
    if !model_dir_ready(model_dir) {
        // Common UX mistake: picker chose the model leaf (…/mlx/Qwen…), not the
        // catalog parent (…/mlx). Leaf has config.json itself; we need a child.
        let leaf_hint = if model_dir.join("config.json").is_file() {
            " — this looks like a model folder; set MLX dir to its parent (…/mlx)"
        } else {
            ""
        };
        return Err(format!(
            "MLX model directory is empty or missing config.json under {}{}",
            model_dir.display(),
            leaf_hint
        ));
    }
    let base = base_path();
    std::fs::create_dir_all(&base).map_err(|e| format!("omlx base path: {e}"))?;

    let pgrn = pgrn_launcher(resource_dir);
    let streaming = pgrn.is_some();
    let bin = pgrn.clone().unwrap_or_else(|| omlx.to_path_buf());
    let mut cmd = Command::new(&bin);
    // Prefer Application Support mlx-runtime over /Applications/oMLX.app.
    if mlx_runtime_ready() {
        if let Some(root) = mlx_runtime_root().to_str() {
            cmd.env("SLIPSTREAM_MLX_RUNTIME", root);
        }
    }
    // Prefetch stays off unless SLIPSTREAM_PGRN_PREFETCH is set in the
    // ambient process environment (opt-in). Auto-enabling overlap before a
    // measured win made prefill slower — see PREFILL_PARITY_PLAN.md.
    let env = pgrn_env
        .cloned()
        .unwrap_or_default()
        .sanitized();
    env.apply_to(&mut cmd);
    cmd.args([
        "serve",
        "--model-dir",
        model_dir.to_str().ok_or("model dir is not UTF-8")?,
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
        "--base-path",
        base.to_str().ok_or("base path is not UTF-8")?,
    ]);
    for a in memory_guard_cli_args(memory_guard_gb, memory_guard_off) {
        cmd.arg(a);
    }
    // MCP default OFF — only when Settings/env path is non-empty.
    for a in mcp_cli_args(&env.mcp_config) {
        cmd.arg(a);
    }
    cmd.args(["--max-concurrent-requests", "1"]);
    // Stock resident oMLX: keep KV SSD off until Disk-Gates know about it.
    // PGRN streaming needs in-memory KV reuse — otherwise every turn re-prefills.
    if !streaming {
        cmd.arg("--no-cache");
    }
    Ok((cmd, streaming))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn empty_dir_is_not_ready() {
        let dir = std::env::temp_dir().join(format!("slip-mlx-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(!model_dir_ready(&dir));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_subdir_with_config_is_ready() {
        let dir = std::env::temp_dir().join(format!("slip-mlx-ready-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let model = dir.join("Qwen-test");
        fs::create_dir_all(&model).unwrap();
        fs::write(model.join("config.json"), "{}").unwrap();
        assert!(model_dir_ready(&dir));
        assert_eq!(model_sidecar_counts(&dir), (1, 0));
        fs::write(model.join("experts.pgrn"), b"PGRN").unwrap();
        assert_eq!(model_sidecar_counts(&dir), (1, 1));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkout_launcher_ready_when_lib_built() {
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../tools/pgrn-mlx/run_omlx_pgrn.sh");
        let lib = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../tools/pgrn-mlx/native/build/libpgrn_host.dylib");
        if lib.is_file() && script.is_file() {
            assert!(launcher_is_ready(&script));
            assert!(pgrn_launcher(None).is_some());
        }
    }

    #[test]
    fn bootstrap_script_resolves_in_checkout() {
        let s = bootstrap_script(None);
        assert!(s.is_some(), "expected the staged MLX bootstrap script");
        assert!(s.unwrap().ends_with("bootstrap_mlx_runtime.sh"));
    }

    #[test]
    fn resolve_backend_explicit_is_sticky() {
        assert_eq!(resolve_backend("metal", 0, true), "metal");
        assert_eq!(resolve_backend("metal", AUTO_PREFILL_CHARS + 1, true), "metal");
        assert_eq!(resolve_backend("mlx", 0, false), "mlx");
        assert_eq!(resolve_backend("mlx", AUTO_PREFILL_CHARS + 1, false), "mlx");
    }

    #[test]
    fn resolve_backend_auto_short_warm_prefers_mlx_with_experts() {
        assert_eq!(resolve_backend("auto", 100, true), "mlx");
        assert_eq!(resolve_backend("heuristic", 0, true), "mlx");
        assert_eq!(resolve_backend("auto", AUTO_PREFILL_CHARS - 1, true), "mlx");
    }

    #[test]
    fn resolve_backend_auto_long_prefill_prefers_metal() {
        assert_eq!(resolve_backend("auto", AUTO_PREFILL_CHARS, true), "metal");
        assert_eq!(resolve_backend("heuristic", AUTO_PREFILL_CHARS * 2, true), "metal");
    }

    #[test]
    fn resolve_backend_auto_without_experts_falls_back_metal() {
        assert_eq!(resolve_backend("auto", 50, false), "metal");
        assert_eq!(resolve_backend("heuristic", 50, false), "metal");
    }

    #[test]
    fn pgrn_mlx_env_defaults_match_balanced_path() {
        let e = PgrnMlxEnv::default();
        let pairs: std::collections::HashMap<_, _> = e.env_pairs().into_iter().collect();
        assert_eq!(pairs.get("SLIPSTREAM_PGRN_PROFILE").map(String::as_str), Some("balanced"));
        // Interactive-safe: touch default; benches pin mlock for ~18.9 tok/s.
        assert_eq!(pairs.get("SLIPSTREAM_PGRN_RESIDENCY").map(String::as_str), Some("touch"));
        assert_eq!(pairs.get("SLIPSTREAM_PGRN_KEEP_HOT").map(String::as_str), Some("1"));
        assert_eq!(pairs.get("SLIPSTREAM_PGRN_WARMUP").map(String::as_str), Some("1"));
        assert_eq!(pairs.get("SLIPSTREAM_PGRN_COLD_IO_WIDTH").map(String::as_str), Some("0"));
        assert_eq!(pairs.get("SLIPSTREAM_PGRN_ONLINE").map(String::as_str), Some("0"));
        assert!(!pairs.contains_key("SLIPSTREAM_PGRN_L3"));
        assert!(!pairs.contains_key("SLIPSTREAM_PGRN_PEER_BASE"));
        assert!(!pairs.contains_key("OMLX_MCP_CONFIG"));
    }

    #[test]
    fn pgrn_mlx_env_mlock_still_opt_in() {
        let e = PgrnMlxEnv::from_parts("balanced", "mlock", true, true, false, "", "");
        assert_eq!(e.residency, "mlock");
        let pairs: std::collections::HashMap<_, _> = e.env_pairs().into_iter().collect();
        assert_eq!(pairs.get("SLIPSTREAM_PGRN_RESIDENCY").map(String::as_str), Some("mlock"));
        assert_eq!(pairs.get("SLIPSTREAM_PGRN_COLD_IO_WIDTH").map(String::as_str), Some("0"));
    }

    #[test]
    fn pgrn_mlx_env_sanitizes_and_enables_peer_l3() {
        let e = PgrnMlxEnv::from_parts(
            "FAST",
            "Touch",
            false,
            false,
            true,
            "  http://192.168.1.10:8765  ",
            "",
        );
        assert_eq!(e.profile, "fast");
        assert_eq!(e.residency, "touch");
        let pairs: std::collections::HashMap<_, _> = e.env_pairs().into_iter().collect();
        assert_eq!(pairs.get("SLIPSTREAM_PGRN_PROFILE").map(String::as_str), Some("fast"));
        assert_eq!(pairs.get("SLIPSTREAM_PGRN_RESIDENCY").map(String::as_str), Some("touch"));
        assert_eq!(pairs.get("SLIPSTREAM_PGRN_KEEP_HOT").map(String::as_str), Some("0"));
        assert_eq!(pairs.get("SLIPSTREAM_PGRN_WARMUP").map(String::as_str), Some("0"));
        assert_eq!(pairs.get("SLIPSTREAM_PGRN_ONLINE").map(String::as_str), Some("1"));
        assert_eq!(pairs.get("SLIPSTREAM_PGRN_L3").map(String::as_str), Some("peer"));
        assert_eq!(
            pairs.get("SLIPSTREAM_PGRN_PEER_BASE").map(String::as_str),
            Some("http://192.168.1.10:8765")
        );
        assert!(!pairs.contains_key("OMLX_MCP_CONFIG"));
    }

    #[test]
    fn pgrn_mlx_env_mcp_config_opt_in() {
        let e = PgrnMlxEnv::from_parts(
            "balanced",
            "touch",
            true,
            true,
            false,
            "",
            "  /tmp/slipstream-mcp.json  ",
        );
        assert_eq!(e.mcp_config, "/tmp/slipstream-mcp.json");
        let pairs: std::collections::HashMap<_, _> = e.env_pairs().into_iter().collect();
        assert_eq!(
            pairs.get("OMLX_MCP_CONFIG").map(String::as_str),
            Some("/tmp/slipstream-mcp.json")
        );
        assert_eq!(
            mcp_cli_args(&e.mcp_config),
            vec![
                "--mcp-config".to_string(),
                "/tmp/slipstream-mcp.json".to_string()
            ]
        );
    }

    #[test]
    fn pgrn_mlx_env_unknown_profile_falls_back_balanced() {
        let e = PgrnMlxEnv::from_parts("turbo", "banana", true, true, false, "", "");
        assert_eq!(e.profile, "balanced");
        assert_eq!(e.residency, "touch");
    }

    #[test]
    fn pgrn_mlx_env_accepts_internal_contract_profile() {
        let e = PgrnMlxEnv::from_parts("CONTRACT", "touch", true, true, false, "", "");
        assert_eq!(e.profile, "contract");
        let pairs: std::collections::HashMap<_, _> = e.env_pairs().into_iter().collect();
        assert_eq!(
            pairs.get("SLIPSTREAM_PGRN_PROFILE").map(String::as_str),
            Some("contract")
        );
    }

    #[test]
    fn mcp_cli_empty_is_off() {
        assert!(mcp_cli_args("").is_empty());
        assert!(mcp_cli_args("   ").is_empty());
    }

    #[test]
    fn mcp_config_path_ok_absolute_or_empty() {
        assert!(mcp_config_path_ok(""));
        assert!(mcp_config_path_ok("   "));
        assert!(mcp_config_path_ok("/tmp/mcp.json"));
        assert!(mcp_config_path_ok("C:\\Users\\a\\mcp.json"));
        assert!(mcp_config_path_ok("D:/mcp.yaml"));
        assert!(!mcp_config_path_ok("mcp.json"));
        assert!(!mcp_config_path_ok("./mcp.json"));
        assert!(!mcp_config_path_ok("relative/mcp.json"));
    }

    #[test]
    fn memory_guard_cli_default_uses_gb() {
        let args = memory_guard_cli_args(33.0, false);
        assert_eq!(args, vec!["--memory-guard-gb".to_string(), "33.0".to_string()]);
    }

    #[test]
    fn memory_guard_cli_off_escape() {
        let args = memory_guard_cli_args(33.0, true);
        assert_eq!(args, vec!["--memory-guard".to_string(), "off".to_string()]);
    }

    #[test]
    fn resident_refuse_msg_mentions_wired_cap_and_free() {
        let msg = mlx_resident_refuse_msg(28.0, 33.0, 3.0, 5.5);
        assert!(msg.contains("RAM too low"));
        assert!(msg.contains("28.0"));
        assert!(msg.contains("5.5"));
        assert!(msg.contains("~28 GiB"));
        assert!(msg.contains("Memory guard off"));
        assert!(msg.contains("experts.pgrn"));
    }

    #[test]
    fn critical_free_refuse_msg_actionable() {
        let msg = mlx_critical_free_refuse_msg(2.1, MLX_MIN_FREE_GIB);
        assert!(msg.contains("2.1"));
        assert!(msg.contains("4.0"));
        assert!(msg.contains("prefill_memory_exceeded"));
        assert!(msg.contains("Memory guard off"));
    }

    #[test]
    fn low_free_soft_tip_only_in_warn_band() {
        assert!(mlx_low_free_soft_tip(8.0, "touch").is_none());
        assert!(mlx_low_free_soft_tip(0.0, "touch").is_none());
        // Below critical floor: refuse owns UX — no success tip.
        assert!(mlx_low_free_soft_tip(2.0, "touch").is_none());
        assert!(mlx_low_free_soft_tip(3.99, "mlock").is_none());
        let tip = mlx_low_free_soft_tip(5.0, "touch").expect("warn band");
        assert!(tip.contains("5.0"));
        assert!(tip.contains("8"));
        assert!(!tip.contains("risky"));
        let mlock = mlx_low_free_soft_tip(5.0, "mlock").expect("warn band");
        assert!(mlock.contains("prefer touch"));
        // Boundaries: inclusive MIN, exclusive WARN.
        assert!(mlx_low_free_soft_tip(4.0, "touch").is_some());
        assert!(mlx_low_free_soft_tip(7.99, "touch").is_some());
        // Case-insensitive mlock note.
        let mlock_up = mlx_low_free_soft_tip(4.5, "MLOCK").expect("warn band");
        assert!(mlock_up.contains("prefer touch"));
        assert!(mlx_low_free_soft_tip(4.5, "TOUCH").expect("tip").contains("4.5"));
        assert!(!mlx_low_free_soft_tip(4.5, "TOUCH").unwrap().contains("risky"));
    }

    #[test]
    fn residency_aliases_and_unknown_default_touch() {
        assert_eq!(PgrnMlxEnv::from_parts("balanced", "mlock", true, true, false, "", "").residency, "mlock");
        assert_eq!(PgrnMlxEnv::from_parts("balanced", "1", true, true, false, "", "").residency, "mlock");
        assert_eq!(PgrnMlxEnv::from_parts("balanced", "YES", true, true, false, "", "").residency, "mlock");
        assert_eq!(PgrnMlxEnv::from_parts("balanced", "True", true, true, false, "", "").residency, "mlock");
        assert_eq!(PgrnMlxEnv::from_parts("balanced", "off", true, true, false, "", "").residency, "off");
        assert_eq!(PgrnMlxEnv::from_parts("balanced", "0", true, true, false, "", "").residency, "off");
        assert_eq!(PgrnMlxEnv::from_parts("balanced", "NO", true, true, false, "", "").residency, "off");
        assert_eq!(PgrnMlxEnv::from_parts("balanced", "", true, true, false, "", "").residency, "touch");
        assert_eq!(PgrnMlxEnv::from_parts("balanced", "  ", true, true, false, "", "").residency, "touch");
        assert_eq!(PgrnMlxEnv::from_parts("balanced", "banana", true, true, false, "", "").residency, "touch");
        assert_eq!(PgrnMlxEnv::from_parts("balanced", "  Touch  ", true, true, false, "", "").residency, "touch");
        assert_eq!(PgrnMlxEnv::from_parts("balanced", "  MLOCK ", true, true, false, "", "").residency, "mlock");
    }

    #[test]
    fn memory_guard_cli_fractional_and_off_ignores_gb() {
        // `{:.1}` uses ties-to-even: 33.25 → "33.2"; 33.26 rounds up.
        assert_eq!(
            memory_guard_cli_args(33.25, false),
            vec!["--memory-guard-gb".to_string(), "33.2".to_string()]
        );
        assert_eq!(
            memory_guard_cli_args(33.26, false),
            vec!["--memory-guard-gb".to_string(), "33.3".to_string()]
        );
        assert_eq!(
            memory_guard_cli_args(0.0, false),
            vec!["--memory-guard-gb".to_string(), "0.0".to_string()]
        );
        assert_eq!(
            memory_guard_cli_args(99.0, true),
            vec!["--memory-guard".to_string(), "off".to_string()]
        );
    }

    #[test]
    fn cold_io_pinned_zero_for_all_residency_modes() {
        for res in ["touch", "mlock", "off"] {
            let e = PgrnMlxEnv::from_parts("balanced", res, true, true, false, "", "");
            let pairs: std::collections::HashMap<_, _> = e.env_pairs().into_iter().collect();
            assert_eq!(
                pairs.get("SLIPSTREAM_PGRN_COLD_IO_WIDTH").map(String::as_str),
                Some("0"),
                "cold_io must stay 0 for residency={res}"
            );
            assert_eq!(
                pairs.get("SLIPSTREAM_PGRN_RESIDENCY").map(String::as_str),
                Some(res)
            );
        }
    }

    #[test]
    fn resolve_backend_unknown_and_empty_fall_back_metal() {
        assert_eq!(resolve_backend("", 0, true), "metal");
        assert_eq!(resolve_backend("MLX", 0, true), "metal");
        assert_eq!(resolve_backend("unknown", AUTO_PREFILL_CHARS + 1, true), "metal");
        assert_eq!(resolve_backend("cpu", 10, false), "metal");
    }

    #[test]
    fn any_experts_sidecar_detects_nested_pgrn() {
        let dir = std::env::temp_dir().join(format!("slip-mlx-experts-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let model = dir.join("M");
        fs::create_dir_all(&model).unwrap();
        fs::write(model.join("config.json"), "{}").unwrap();
        assert!(!any_experts_sidecar(&dir));
        fs::write(model.join("experts.pgrn"), b"PGRN").unwrap();
        assert!(any_experts_sidecar(&dir));
        assert!(!any_experts_sidecar(&dir.join("missing")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_sidecar_counts_skips_non_models() {
        let dir = std::env::temp_dir().join(format!("slip-mlx-sidecar-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("readme.txt"), "x").unwrap();
        let bare = dir.join("no-config");
        fs::create_dir_all(&bare).unwrap();
        assert_eq!(model_sidecar_counts(&dir), (0, 0));
        assert_eq!(model_sidecar_counts(&dir.join("gone")), (0, 0));
        let _ = fs::remove_dir_all(&dir);
    }
}
