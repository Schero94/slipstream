//! Validated serve argv plans (dry-run friendly; no process spawn).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command};

use p2p_core::BackendKind;

/// Exact argv + env a provider would launch for a real engine.
///
/// Built without spawning. Use [`ServePlan::display`] for dry-run output and
/// [`ServePlan::spawn`] (behind the `launch` feature) to start the child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServePlan {
    pub backend: BackendKind,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Extra environment variables (profile, etc.).
    pub env: Vec<(String, String)>,
}

impl ServePlan {
    /// Shell-ish one-liner: `ENV=… program arg1 arg2 …`
    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        for (k, v) in &self.env {
            parts.push(format!("{k}={v}"));
        }
        parts.push(self.program.display().to_string());
        for a in &self.args {
            if a.contains(char::is_whitespace) {
                parts.push(format!("\"{a}\""));
            } else {
                parts.push(a.clone());
            }
        }
        parts.join(" ")
    }

    /// Argv as `program` followed by args (no env).
    pub fn argv(&self) -> Vec<String> {
        let mut v = Vec::with_capacity(1 + self.args.len());
        v.push(self.program.display().to_string());
        v.extend(self.args.iter().cloned());
        v
    }

    pub fn to_command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd
    }

    /// Port from `--port <n>` in argv, if present.
    pub fn listen_port(&self) -> Option<u16> {
        self.args
            .windows(2)
            .find(|w| w[0] == "--port")
            .and_then(|w| w[1].parse().ok())
    }

    /// Host from `--host <h>` in argv, else `127.0.0.1`.
    pub fn listen_host(&self) -> &str {
        self.args
            .windows(2)
            .find(|w| w[0] == "--host")
            .map(|w| w[1].as_str())
            .unwrap_or("127.0.0.1")
    }

    /// OpenAI-compatible base URL derived from the serve plan (`http://host:port`).
    pub fn http_endpoint(&self) -> Option<String> {
        self.listen_port()
            .map(|p| crate::http::local_endpoint(self.listen_host(), p))
    }

    /// Spawn the planned process. Fails immediately if the binary cannot start;
    /// also fails if the child exits within a short grace period (never hang).
    #[cfg(feature = "launch")]
    pub fn spawn(&self) -> Result<Child, String> {
        use std::process::Stdio;
        use std::thread;
        use std::time::Duration;

        let mut cmd = self.to_command();
        // Inherit so operators see engine logs; do not wait on stdout.
        cmd.stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn {}: {e}", self.program.display()))?;
        // Fail-fast: if the engine dies immediately (bad flags / missing dylib),
        // surface it instead of leaving a dead provider.
        thread::sleep(Duration::from_millis(250));
        match child.try_wait() {
            Ok(Some(status)) => Err(format!(
                "engine exited immediately ({status}); check binary/model paths. planned: {}",
                self.display()
            )),
            Ok(None) => Ok(child),
            Err(e) => Err(format!("engine status check failed: {e}")),
        }
    }

    #[cfg(not(feature = "launch"))]
    pub fn spawn(&self) -> Result<Child, String> {
        let _ = self;
        Err(
            "engine spawn disabled (p2p-engine built without `launch` feature); \
             use --dry-run-engine to print argv, or rebuild with --features launch"
                .into(),
        )
    }
}

/// Explicit provider engine selection (CLI / env / Tauri).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineChoice {
    /// Deterministic mock — default for CI and casual demos.
    Mock,
    /// OS matrix: Mac/Darwin/iOS → MLX; else → Llama.
    Auto,
    Mlx,
    Llama,
}

impl EngineChoice {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mock" => Ok(Self::Mock),
            "auto" | "os" => Ok(Self::Auto),
            "mlx" | "omlx" | "mlx_pgrn" => Ok(Self::Mlx),
            "llama" | "llama_pgrn" | "llamapgrn" | "pgrn" => Ok(Self::Llama),
            other => Err(format!(
                "unknown engine '{other}' (expected mock|auto|mlx|llama)"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::Auto => "auto",
            Self::Mlx => "mlx",
            Self::Llama => "llama",
        }
    }

    pub fn is_mock(self) -> bool {
        matches!(self, Self::Mock)
    }

    /// Resolve to a concrete backend (Auto uses OS matrix).
    pub fn to_backend(self, os: &str) -> Option<BackendKind> {
        match self {
            Self::Mock => None,
            Self::Auto => Some(crate::select_backend(os)),
            Self::Mlx => Some(BackendKind::Mlx),
            Self::Llama => Some(BackendKind::LlamaPgrn),
        }
    }
}

/// Read `SLIPSTREAM_P2P_ENGINE` (or `P2P_ENGINE`) when set.
pub fn engine_choice_from_env() -> Result<Option<EngineChoice>, String> {
    let raw = std::env::var("SLIPSTREAM_P2P_ENGINE")
        .or_else(|_| std::env::var("P2P_ENGINE"));
    match raw {
        Ok(s) if !s.trim().is_empty() => Ok(Some(EngineChoice::parse(&s)?)),
        _ => Ok(None),
    }
}

/// Merge CLI choice with env. Explicit CLI wins; env applies when CLI is mock/default.
pub fn resolve_engine_choice(
    cli: Option<EngineChoice>,
    default_mock: bool,
) -> Result<EngineChoice, String> {
    if let Some(c) = cli {
        return Ok(c);
    }
    if let Some(env) = engine_choice_from_env()? {
        return Ok(env);
    }
    Ok(if default_mock {
        EngineChoice::Mock
    } else {
        EngineChoice::Auto
    })
}

/// Build a validated [`ServePlan`] for a concrete backend (never spawns).
pub fn plan_serve_for_backend(kind: BackendKind) -> Result<ServePlan, String> {
    match kind {
        BackendKind::Mlx => crate::mlx::MlxEngine::from_env().plan_serve_argv(),
        BackendKind::LlamaPgrn => crate::llama::LlamaPgrnEngine::from_env().plan_serve_argv(),
    }
}

/// Plan serve argv for a choice. Mock → error (nothing to launch).
pub fn plan_serve_for_choice(choice: EngineChoice, os: &str) -> Result<ServePlan, String> {
    let kind = choice
        .to_backend(os)
        .ok_or_else(|| "mock engine has no serve process to plan".to_string())?;
    plan_serve_for_backend(kind)
}

/// Launch (spawn) a planned serve. Requires the `launch` feature.
pub fn launch_serve_plan(plan: &ServePlan) -> Result<Child, String> {
    plan.spawn()
}

/// Kill a spawned engine child (best-effort; used on node shutdown).
pub fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Helper for tests: env map from plan.
#[allow(dead_code)]
pub(crate) fn env_map(plan: &ServePlan) -> HashMap<String, String> {
    plan.env.iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_engine_choice_aliases() {
        assert_eq!(EngineChoice::parse("MLX").unwrap(), EngineChoice::Mlx);
        assert_eq!(EngineChoice::parse("llama_pgrn").unwrap(), EngineChoice::Llama);
        assert_eq!(EngineChoice::parse("auto").unwrap(), EngineChoice::Auto);
        assert_eq!(EngineChoice::parse("mock").unwrap(), EngineChoice::Mock);
        assert!(EngineChoice::parse("gpu").is_err());
    }

    #[test]
    fn auto_mac_is_mlx_else_llama() {
        assert_eq!(
            EngineChoice::Auto.to_backend("macos"),
            Some(BackendKind::Mlx)
        );
        assert_eq!(
            EngineChoice::Auto.to_backend("linux"),
            Some(BackendKind::LlamaPgrn)
        );
        assert_eq!(EngineChoice::Mock.to_backend("macos"), None);
    }

    #[test]
    fn serve_plan_display_includes_program_and_args() {
        let plan = ServePlan {
            backend: BackendKind::LlamaPgrn,
            program: PathBuf::from("/usr/bin/llama-server"),
            args: vec!["--port".into(), "8081".into()],
            env: vec![("FOO".into(), "bar".into())],
        };
        let d = plan.display();
        assert!(d.contains("FOO=bar"), "{d}");
        assert!(d.contains("/usr/bin/llama-server"), "{d}");
        assert!(d.contains("--port"), "{d}");
        assert_eq!(
            plan.argv(),
            vec!["/usr/bin/llama-server", "--port", "8081"]
        );
        assert_eq!(plan.listen_port(), Some(8081));
        assert_eq!(
            plan.http_endpoint().as_deref(),
            Some("http://127.0.0.1:8081")
        );
    }

    #[test]
    fn spawn_blocked_without_launch_feature() {
        let plan = ServePlan {
            backend: BackendKind::Mlx,
            program: PathBuf::from("/nonexistent-omlx"),
            args: vec![],
            env: vec![],
        };
        #[cfg(not(feature = "launch"))]
        {
            let err = plan.spawn().unwrap_err();
            assert!(err.contains("launch"), "{err}");
        }
    }
}
