//! Non-Mac llama.cpp / PGRN fork / llama-server adapter.
//!
//! Stub for OpenAI-compatible `llama-server` (or Slipstream llama path).
//! Real process launch is feature-gated (`launch`); default is mock-safe.
//! Prefer [`LlamaPgrnEngine::plan_serve_argv`] for validated dry-run argv.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use p2p_core::{BackendKind, InferenceEngine, JobRequest, JobResult};

use crate::plan::ServePlan;

/// Resolve llama-server binary: override → PATH → common names.
pub fn resolve_llama_server(override_path: &str) -> Result<PathBuf, String> {
    let trimmed = override_path.trim();
    if !trimmed.is_empty() {
        let p = PathBuf::from(trimmed);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!("llama-server binary not found at {trimmed}"));
    }
    for name in ["llama-server", "llama_server"] {
        if let Ok(path) = which(name) {
            return Ok(path);
        }
    }
    Err(
        "llama-server not found. Build the llama.cpp / PGRN fork or set P2P_LLAMA_SERVER_PATH."
            .into(),
    )
}

fn which(name: &str) -> Result<PathBuf, ()> {
    let out = Command::new("sh")
        .args(["-c", &format!("command -v {name}")])
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

/// Build a validated [`ServePlan`] (does not spawn).
pub fn serve_plan(binary: &Path, model: &Path, host: &str, port: u16) -> Result<ServePlan, String> {
    if !binary.is_file() {
        return Err(format!(
            "llama-server binary not found: {} (set P2P_LLAMA_SERVER_PATH)",
            binary.display()
        ));
    }
    if !model.is_file() && !model.is_dir() {
        return Err(format!(
            "llama model path missing: {} (set P2P_LLAMA_MODEL)",
            model.display()
        ));
    }
    Ok(ServePlan {
        backend: BackendKind::LlamaPgrn,
        program: binary.to_path_buf(),
        args: vec![
            "--host".into(),
            host.to_string(),
            "--port".into(),
            port.to_string(),
            "-m".into(),
            model
                .to_str()
                .ok_or("model path is not UTF-8")?
                .to_string(),
        ],
        env: vec![],
    })
}

/// Build a llama-server command (does not spawn).
pub fn serve_command(
    binary: &Path,
    model: &Path,
    host: &str,
    port: u16,
) -> Result<Command, String> {
    Ok(serve_plan(binary, model, host, port)?.to_command())
}

/// Stub llama / PGRN engine for non-Mac nodes.
#[derive(Debug, Clone)]
pub struct LlamaPgrnEngine {
    pub endpoint: Option<String>,
    pub server_path: String,
    pub model_path: PathBuf,
    pub host: String,
    pub port: u16,
}

impl Default for LlamaPgrnEngine {
    fn default() -> Self {
        Self::from_env()
    }
}

impl LlamaPgrnEngine {
    pub fn from_env() -> Self {
        let host = std::env::var("P2P_LLAMA_HOST").unwrap_or_else(|_| "127.0.0.1".into());
        let port = std::env::var("P2P_LLAMA_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8081);
        let endpoint = std::env::var("P2P_LLAMA_ENDPOINT")
            .ok()
            .or_else(|| std::env::var("SLIPSTREAM_LLAMA_ENDPOINT").ok())
            .filter(|s| !s.trim().is_empty())
            .or_else(|| Some(crate::http::local_endpoint(&host, port)));
        Self {
            endpoint,
            server_path: std::env::var("P2P_LLAMA_SERVER_PATH").unwrap_or_default(),
            model_path: std::env::var("P2P_LLAMA_MODEL")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("model.gguf")),
            host,
            port,
        }
    }

    /// Override the HTTP base URL (e.g. after `--spawn-engine`).
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Validated argv plan (no spawn) — preferred for dry-run / tests.
    pub fn plan_serve_argv(&self) -> Result<ServePlan, String> {
        let bin = resolve_llama_server(&self.server_path)?;
        serve_plan(&bin, &self.model_path, &self.host, self.port)
    }

    pub fn plan_serve(&self) -> Result<Command, String> {
        Ok(self.plan_serve_argv()?.to_command())
    }

    pub fn launch_serve(&self) -> Result<Child, String> {
        let plan = self.plan_serve_argv()?;
        plan.spawn()
    }
}

impl InferenceEngine for LlamaPgrnEngine {
    fn infer(&self, job: &JobRequest) -> JobResult {
        match &self.endpoint {
            Some(url) if !url.trim().is_empty() => {
                crate::http::infer_via_http(url, BackendKind::LlamaPgrn, job)
            }
            _ => JobResult::failure(
                &job.job_id,
                "llama engine not configured; set P2P_LLAMA_ENDPOINT or use MockEngine",
            ),
        }
    }

    fn backend_kind(&self) -> Option<BackendKind> {
        Some(BackendKind::LlamaPgrn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn llama_infer_unavailable_without_endpoint() {
        let eng = LlamaPgrnEngine {
            endpoint: None,
            ..LlamaPgrnEngine::from_env()
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
        assert_eq!(eng.backend_kind(), Some(BackendKind::LlamaPgrn));
    }

    #[test]
    fn launch_serve_fails_clearly_when_missing() {
        let eng = LlamaPgrnEngine {
            server_path: "/nonexistent-llama-server".into(),
            model_path: PathBuf::from("/nonexistent/model.gguf"),
            ..LlamaPgrnEngine::from_env()
        };
        let err = eng.launch_serve().unwrap_err();
        assert!(
            err.contains("not found") || err.contains("missing") || err.contains("launch"),
            "{err}"
        );
    }

    #[test]
    fn serve_plan_builds_args() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("model.gguf");
        fs::write(&model, b"gguf").unwrap();
        let bin = dir.path().join("llama-server");
        fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&bin).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&bin, perms).unwrap();
        }
        let plan = serve_plan(&bin, &model, "127.0.0.1", 8081).unwrap();
        assert_eq!(plan.backend, BackendKind::LlamaPgrn);
        assert!(plan.args.iter().any(|a| a == "--port"));
        assert!(plan.args.iter().any(|a| a == "8081"));
        assert!(plan.args.iter().any(|a| a == "-m"));
        assert!(plan.display().contains("8081"));
    }

    #[test]
    fn serve_plan_fails_when_model_missing() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("llama-server");
        fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&bin).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&bin, perms).unwrap();
        }
        let err = serve_plan(&bin, &dir.path().join("nope.gguf"), "127.0.0.1", 1).unwrap_err();
        assert!(err.contains("missing"), "{err}");
    }

    #[test]
    fn serve_command_builds_args() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("model.gguf");
        fs::write(&model, b"gguf").unwrap();
        let bin = dir.path().join("llama-server");
        fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&bin).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&bin, perms).unwrap();
        }
        let cmd = serve_command(&bin, &model, "127.0.0.1", 8081).unwrap();
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|a| a == "--port"));
        assert!(args.iter().any(|a| a == "8081"));
        assert!(args.iter().any(|a| a == "-m"));
    }
}
