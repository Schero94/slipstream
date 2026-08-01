//! Mac oMLX / MLX PGRN engine adapter.
//!
//! Launcher wiring mirrors `apps/peregrine-control/src-tauri/src/mlx.rs` and
//! `tools/pgrn-mlx/run_omlx_pgrn.sh`:
//! - resolve `omlx` (override → PATH → Homebrew)
//! - prefer PGRN launcher when `libpgrn_host.dylib` exists
//! - build `serve --model-dir … --host 127.0.0.1 --port …` args
//!
//! **Default is mock-safe:** no child process is spawned unless the `launch`
//! feature is enabled. Without `launch`, [`MlxEngine::launch_serve`] returns a
//! clear error. Prefer [`MlxEngine::plan_serve_argv`] / dry-run for CI.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use p2p_core::{BackendKind, InferenceEngine, JobRequest, JobResult};

use crate::plan::ServePlan;

/// Default model-dir used by Slipstream when unset (same as mlx.rs).
/// Canonical home: internal SSD `~/Modelle/mlx` (external Crucial = staging only).
pub fn default_model_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Modelle/mlx");
    }
    PathBuf::from("/tmp/Modelle/mlx")
}

/// Application-support root so Slipstream does not share `~/.omlx`.
pub fn base_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Library/Application Support/Slipstream/omlx");
    }
    PathBuf::from("/tmp/slipstream/omlx")
}

/// Resolve `omlx`: explicit override, then PATH, then Homebrew.
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
    let brew = PathBuf::from("/opt/homebrew/bin/omlx");
    if brew.is_file() {
        return Ok(brew);
    }
    Err(
        "omlx not found. Install with `brew install omlx` or set P2P_OMLX_PATH / Settings."
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

/// Launcher that sets `SLIPSTREAM_PGRN*` via `tools/pgrn-mlx/run_omlx_pgrn.sh`.
///
/// Env override wins; then repo-relative candidates. Requires `libpgrn_host.dylib`
/// beside the script (same gate as Slipstream `mlx.rs`).
pub fn pgrn_launcher() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SLIPSTREAM_OMLX_LAUNCHER") {
        let path = PathBuf::from(p.trim());
        if path.is_file() {
            return Some(path);
        }
    }
    let candidates = [
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/pgrn-mlx/run_omlx_pgrn.sh"),
    ];
    for c in candidates {
        if !c.is_file() {
            continue;
        }
        let Some(parent) = c.parent() else {
            continue;
        };
        let lib = parent.join("native/build/libpgrn_host.dylib");
        if lib.is_file() {
            return Some(c);
        }
    }
    None
}

/// Build a validated [`ServePlan`] (does not spawn).
pub fn serve_plan(
    omlx: &Path,
    model_dir: &Path,
    port: u16,
    memory_guard_gb: f64,
) -> Result<ServePlan, String> {
    if !model_dir.is_dir() {
        return Err(format!(
            "MLX model directory missing: {} (set P2P_MLX_MODEL_DIR)",
            model_dir.display()
        ));
    }
    if !model_dir_ready(model_dir) {
        return Err(format!(
            "MLX model directory is empty or missing config.json under {} (set P2P_MLX_MODEL_DIR)",
            model_dir.display()
        ));
    }
    let base = base_path();
    std::fs::create_dir_all(&base).map_err(|e| format!("omlx base path: {e}"))?;

    let pgrn = pgrn_launcher();
    let program = pgrn.clone().unwrap_or_else(|| omlx.to_path_buf());
    if !program.is_file() {
        return Err(format!("MLX serve binary not found: {}", program.display()));
    }

    let mut env = Vec::new();
    if std::env::var_os("SLIPSTREAM_PGRN_PROFILE").is_none() {
        env.push(("SLIPSTREAM_PGRN_PROFILE".into(), "balanced".into()));
    }

    let mut args = vec![
        "serve".into(),
        "--model-dir".into(),
        model_dir
            .to_str()
            .ok_or("model dir is not UTF-8")?
            .to_string(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string(),
        "--base-path".into(),
        base.to_str().ok_or("base path is not UTF-8")?.to_string(),
        "--memory-guard-gb".into(),
        format!("{memory_guard_gb:.1}"),
        "--max-concurrent-requests".into(),
        "1".into(),
    ];
    // Stock resident oMLX: keep KV SSD off until Disk-Gates know about it.
    if pgrn.is_none() {
        args.push("--no-cache".into());
    }

    Ok(ServePlan {
        backend: BackendKind::Mlx,
        program,
        args,
        env,
    })
}

/// Build the serve command. Prefers the PGRN fork launcher; else stock omlx.
///
/// Does **not** spawn — call [`ServePlan::spawn`] only behind the `launch` feature.
pub fn serve_command(
    omlx: &Path,
    model_dir: &Path,
    port: u16,
    memory_guard_gb: f64,
) -> Result<Command, String> {
    Ok(serve_plan(omlx, model_dir, port, memory_guard_gb)?.to_command())
}

/// Stub / launcher-wired MLX engine. Default path never spawns a process.
#[derive(Debug, Clone)]
pub struct MlxEngine {
    /// OpenAI-compatible HTTP endpoint once a server is running.
    pub endpoint: Option<String>,
    pub omlx_path: String,
    pub model_dir: PathBuf,
    pub port: u16,
    pub memory_guard_gb: f64,
}

impl Default for MlxEngine {
    fn default() -> Self {
        Self::from_env()
    }
}

impl MlxEngine {
    pub fn from_env() -> Self {
        let port = std::env::var("P2P_MLX_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8080);
        let endpoint = std::env::var("P2P_MLX_ENDPOINT")
            .ok()
            .or_else(|| std::env::var("SLIPSTREAM_MLX_ENDPOINT").ok())
            .filter(|s| !s.trim().is_empty())
            // Default matches Slipstream / oMLX listen port (8080).
            .or_else(|| Some(crate::http::local_endpoint("127.0.0.1", port)));
        Self {
            endpoint,
            omlx_path: std::env::var("P2P_OMLX_PATH").unwrap_or_default(),
            model_dir: std::env::var("P2P_MLX_MODEL_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| default_model_dir()),
            port,
            memory_guard_gb: std::env::var("P2P_MLX_MEMORY_GUARD_GB")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3.0),
        }
    }

    /// Override the HTTP base URL (e.g. after `--spawn-engine`).
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Validated argv plan (no spawn) — preferred for dry-run / tests.
    pub fn plan_serve_argv(&self) -> Result<ServePlan, String> {
        let omlx = resolve_omlx(&self.omlx_path)?;
        serve_plan(&omlx, &self.model_dir, self.port, self.memory_guard_gb)
    }

    /// Build (do not run) the serve command — useful for tests / Slipstream wiring.
    pub fn plan_serve(&self) -> Result<Command, String> {
        Ok(self.plan_serve_argv()?.to_command())
    }

    /// Feature-gated real launch. Without `launch`, returns an error string.
    pub fn launch_serve(&self) -> Result<Child, String> {
        let plan = self.plan_serve_argv()?;
        plan.spawn()
    }
}

impl InferenceEngine for MlxEngine {
    fn infer(&self, job: &JobRequest) -> JobResult {
        match &self.endpoint {
            Some(url) if !url.trim().is_empty() => {
                crate::http::infer_via_http(url, BackendKind::Mlx, job)
            }
            _ => JobResult::failure(
                &job.job_id,
                "MLX engine not configured; set P2P_MLX_ENDPOINT or use MockEngine",
            ),
        }
    }

    fn backend_kind(&self) -> Option<BackendKind> {
        Some(BackendKind::Mlx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn empty_dir_is_not_ready() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!model_dir_ready(dir.path()));
    }

    #[test]
    fn subdir_with_config_is_ready() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("Qwen-test");
        fs::create_dir_all(&model).unwrap();
        fs::write(model.join("config.json"), "{}").unwrap();
        assert!(model_dir_ready(dir.path()));
    }

    #[test]
    fn mlx_infer_unavailable_without_endpoint() {
        let eng = MlxEngine {
            endpoint: None,
            ..MlxEngine::from_env()
        };
        let job = JobRequest {
            job_id: "1".into(),
            model: "m".into(),
            system: String::new(),
            prompt: "x".into(),
            max_tokens: 1,
        };
        let r = eng.infer(&job);
        assert!(!r.ok);
        assert_eq!(eng.backend_kind(), Some(BackendKind::Mlx));
    }

    #[test]
    fn mlx_infer_posts_to_local_mock_http() {
        use crate::http::{HttpEngine, DEFAULT_API_KEY};
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            tx.send(()).ok();
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                headers.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let cl = headers.lines().find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|s| s.trim().parse().ok())
            });
            if let Some(n) = cl {
                let mut buf = vec![0u8; n];
                reader.read_exact(&mut buf).ok();
            }
            let body = r#"{"choices":[{"message":{"content":"mlx-ok"}}],"usage":{"completion_tokens":1}}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).unwrap();
        });
        rx.recv().unwrap();

        // Sanity: HttpEngine reaches the same mock; then MlxEngine adapter.
        let _ = HttpEngine::new(format!("http://{addr}"))
            .with_api_key(DEFAULT_API_KEY)
            .with_timeouts(Duration::from_secs(2), Duration::from_secs(5));

        let eng = MlxEngine {
            endpoint: Some(format!("http://{addr}")),
            ..MlxEngine::from_env()
        };
        let job = JobRequest {
            job_id: "mlx-http".into(),
            model: "slipstream".into(),
            system: String::new(),
            prompt: "ping".into(),
            max_tokens: 8,
        };
        let r = eng.infer(&job);
        handle.join().unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.text, "mlx-ok");
        assert_eq!(r.tokens, 1);
    }

    #[test]
    fn launch_serve_blocked_without_feature_or_missing_bins() {
        let eng = MlxEngine {
            omlx_path: "/nonexistent-omlx-binary".into(),
            model_dir: PathBuf::from("/nonexistent-model-dir"),
            ..MlxEngine::from_env()
        };
        let err = eng.launch_serve().unwrap_err();
        // Either missing binary/model (validation) or launch feature gate.
        assert!(
            err.contains("not found")
                || err.contains("missing")
                || err.contains("launch")
                || err.contains("empty"),
            "{err}"
        );
    }

    #[test]
    fn serve_plan_builds_when_model_dir_ready() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("m");
        fs::create_dir_all(&model).unwrap();
        fs::write(model.join("config.json"), "{}").unwrap();

        let fake = dir.path().join("fake-omlx");
        fs::write(&fake, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&fake).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&fake, perms).unwrap();
        }

        let plan = serve_plan(&fake, dir.path(), 9090, 3.0).unwrap();
        assert_eq!(plan.backend, BackendKind::Mlx);
        assert!(plan.args.iter().any(|a| a == "serve"));
        assert!(plan.args.iter().any(|a| a == "--port"));
        assert!(plan.args.iter().any(|a| a == "9090"));
        let display = plan.display();
        assert!(display.contains("9090"), "{display}");
    }

    #[test]
    fn serve_plan_fails_clearly_when_model_missing() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake-omlx");
        fs::write(&fake, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&fake).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&fake, perms).unwrap();
        }
        let err = serve_plan(&fake, dir.path(), 9090, 3.0).unwrap_err();
        assert!(err.contains("empty") || err.contains("missing"), "{err}");
    }

    #[test]
    fn serve_command_builds_when_model_dir_ready() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("m");
        fs::create_dir_all(&model).unwrap();
        fs::write(model.join("config.json"), "{}").unwrap();

        let fake = dir.path().join("fake-omlx");
        fs::write(&fake, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&fake).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&fake, perms).unwrap();
        }

        let cmd = serve_command(&fake, dir.path(), 9090, 3.0).unwrap();
        let prog = cmd.get_program().to_string_lossy();
        assert!(!prog.is_empty());
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|a| a == "serve"));
        assert!(args.iter().any(|a| a == "--port"));
        assert!(args.iter().any(|a| a == "9090"));
    }
}
