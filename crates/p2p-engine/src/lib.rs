//! `p2p-engine` — inference adapters for Slipstream P2P.
//!
//! - [`MockEngine`] (from `p2p-core`): deterministic CI path (default)
//! - [`MlxEngine`]: macOS oMLX / PGRN launcher + HTTP `/v1/chat/completions`
//! - [`LlamaPgrnEngine`]: non-Mac llama-server / PGRN + HTTP `/v1/chat/completions`
//! - [`HttpEngine`]: shared OpenAI-compatible client used by the adapters above
//!
//! [`select_engine`]: Mac/Darwin/iOS → [`BackendKind::Mlx`]; else → [`BackendKind::LlamaPgrn`].
//!
//! Real process spawn is behind the `launch` feature (default off = mock-safe).
//! Prefer [`plan_serve_for_choice`] / `--dry-run-engine` to print the exact argv
//! that would be launched; use [`launch_serve_plan`] only with `launch` + explicit
//! `--engine mlx|llama` (or `SLIPSTREAM_P2P_ENGINE`).
//!
//! When not mock, sealed jobs POST to `{endpoint}/v1/chat/completions`
//! (`P2P_MLX_ENDPOINT` / `P2P_LLAMA_ENDPOINT`, else `http://127.0.0.1:{port}`).

#![forbid(unsafe_code)]

pub mod http;
pub mod llama;
pub mod mlx;
pub mod plan;

pub use http::{
    chat_completions_url, http_chat_completions, local_endpoint, HttpEngine, DEFAULT_API_KEY,
};
pub use llama::LlamaPgrnEngine;
pub use mlx::MlxEngine;
pub use p2p_core::{
    select_backend, BackendKind, Capability, InferenceEngine, JobRequest, JobResult, MockEngine,
};
pub use plan::{
    engine_choice_from_env, launch_serve_plan, plan_serve_for_backend, plan_serve_for_choice,
    resolve_engine_choice, stop_child, EngineChoice, ServePlan,
};

/// Select the production engine backend for `(os, caps)`.
///
/// Matrix (MVP):
/// | OS contains              | Backend              |
/// |--------------------------|----------------------|
/// | `macos` / `darwin` / `ios` | [`BackendKind::Mlx`] |
/// | anything else            | [`BackendKind::LlamaPgrn`] |
///
/// Empty `os` falls back to `caps.os`. Hardware fields on `caps` are reserved
/// for future model-aware selection; OS drives the MVP decision (same as
/// [`select_backend`]).
pub fn select_engine(os: &str, caps: &Capability) -> BackendKind {
    let os = if os.trim().is_empty() {
        caps.os.as_str()
    } else {
        os
    };
    let _ = (caps.ram_gib, caps.vram_gib, &caps.models);
    select_backend(os)
}

/// Construct an adapter for a selected backend (env-configured stubs by default).
pub fn build_engine(kind: BackendKind) -> Box<dyn InferenceEngine> {
    match kind {
        BackendKind::Mlx => Box::new(MlxEngine::from_env()),
        BackendKind::LlamaPgrn => Box::new(LlamaPgrnEngine::from_env()),
    }
}

/// Open the engine for a node: mock when `force_mock`, else OS/caps selection.
pub fn open_engine(os: &str, caps: &Capability, force_mock: bool) -> Box<dyn InferenceEngine> {
    if force_mock {
        return Box::new(MockEngine);
    }
    build_engine(select_engine(os, caps))
}

/// Open an engine for an explicit [`EngineChoice`] (CLI / env / Tauri).
pub fn open_engine_for_choice(
    choice: EngineChoice,
    os: &str,
    caps: &Capability,
) -> Box<dyn InferenceEngine> {
    open_engine_for_choice_at(choice, os, caps, None)
}

/// Like [`open_engine_for_choice`], optionally overriding the HTTP base URL
/// (e.g. from a spawned [`ServePlan::http_endpoint`]).
pub fn open_engine_for_choice_at(
    choice: EngineChoice,
    os: &str,
    caps: &Capability,
    endpoint: Option<&str>,
) -> Box<dyn InferenceEngine> {
    match choice {
        EngineChoice::Mock => Box::new(MockEngine),
        EngineChoice::Auto => {
            let kind = select_engine(os, caps);
            apply_endpoint(build_engine(kind), endpoint, kind)
        }
        EngineChoice::Mlx => {
            let mut eng = MlxEngine::from_env();
            if let Some(url) = endpoint.filter(|u| !u.trim().is_empty()) {
                eng = eng.with_endpoint(url);
            }
            Box::new(eng)
        }
        EngineChoice::Llama => {
            let mut eng = LlamaPgrnEngine::from_env();
            if let Some(url) = endpoint.filter(|u| !u.trim().is_empty()) {
                eng = eng.with_endpoint(url);
            }
            Box::new(eng)
        }
    }
}

fn apply_endpoint(
    engine: Box<dyn InferenceEngine>,
    endpoint: Option<&str>,
    kind: BackendKind,
) -> Box<dyn InferenceEngine> {
    let Some(url) = endpoint.filter(|u| !u.trim().is_empty()) else {
        return engine;
    };
    match kind {
        BackendKind::Mlx => Box::new(MlxEngine::from_env().with_endpoint(url)),
        BackendKind::LlamaPgrn => Box::new(LlamaPgrnEngine::from_env().with_endpoint(url)),
    }
}

/// Whether the `launch` cargo feature is compiled in (spawn allowed).
pub fn launch_feature_enabled() -> bool {
    cfg!(feature = "launch")
}

#[cfg(test)]
mod tests {
    use super::*;
    use p2p_core::local_capability;
    use std::fs;

    fn caps(os: &str) -> Capability {
        local_capability(os, 36, 0, vec!["qwen3-30b".into()])
    }

    #[test]
    fn select_engine_macos_variants_are_mlx() {
        for os in ["macos", "macOS", "Darwin", "darwin", "ios", "iOS"] {
            assert_eq!(
                select_engine(os, &caps("linux")),
                BackendKind::Mlx,
                "os={os}"
            );
        }
    }

    #[test]
    fn select_engine_non_mac_is_llama() {
        for os in ["linux", "Linux", "windows", "Windows", "freebsd", "android"] {
            assert_eq!(
                select_engine(os, &caps("macos")),
                BackendKind::LlamaPgrn,
                "os={os}"
            );
        }
    }

    #[test]
    fn select_engine_empty_os_uses_caps_os() {
        assert_eq!(select_engine("", &caps("macos")), BackendKind::Mlx);
        assert_eq!(select_engine("  ", &caps("linux")), BackendKind::LlamaPgrn);
    }

    #[test]
    fn select_engine_matrix_table() {
        // Explicit selection matrix for the report / CI.
        let cases = [
            ("macos", BackendKind::Mlx),
            ("Darwin", BackendKind::Mlx),
            ("ios", BackendKind::Mlx),
            ("linux", BackendKind::LlamaPgrn),
            ("windows", BackendKind::LlamaPgrn),
        ];
        for (os, want) in cases {
            assert_eq!(select_engine(os, &caps(os)), want);
            assert_eq!(select_backend(os), want);
        }
    }

    #[test]
    fn open_engine_force_mock_is_deterministic() {
        let c = caps("macos");
        let eng = open_engine("macos", &c, true);
        assert!(eng.backend_kind().is_none());
        let job = JobRequest {
            job_id: "j".into(),
            model: "m".into(),
            system: "s".into(),
            prompt: "hello".into(),
            max_tokens: 2,
        };
        let a = eng.infer(&job);
        let b = eng.infer(&job);
        assert_eq!(a, b);
        assert!(a.ok);
        assert_eq!(a.tokens, 2);
    }

    #[test]
    fn build_engine_kinds() {
        assert_eq!(
            build_engine(BackendKind::Mlx).backend_kind(),
            Some(BackendKind::Mlx)
        );
        assert_eq!(
            build_engine(BackendKind::LlamaPgrn).backend_kind(),
            Some(BackendKind::LlamaPgrn)
        );
    }

    #[test]
    fn open_engine_macos_without_mock_is_mlx() {
        let c = caps("macos");
        let eng = open_engine("macos", &c, false);
        assert_eq!(eng.backend_kind(), Some(BackendKind::Mlx));
    }

    #[test]
    fn open_engine_linux_without_mock_is_llama() {
        let c = caps("linux");
        let eng = open_engine("linux", &c, false);
        assert_eq!(eng.backend_kind(), Some(BackendKind::LlamaPgrn));
    }

    #[test]
    fn open_engine_for_choice_respects_explicit_override() {
        let c = caps("linux");
        // Explicit mlx on linux (override OS matrix).
        assert_eq!(
            open_engine_for_choice(EngineChoice::Mlx, "linux", &c).backend_kind(),
            Some(BackendKind::Mlx)
        );
        // Explicit llama on macos.
        assert_eq!(
            open_engine_for_choice(EngineChoice::Llama, "macos", &c).backend_kind(),
            Some(BackendKind::LlamaPgrn)
        );
        assert!(open_engine_for_choice(EngineChoice::Mock, "macos", &c)
            .backend_kind()
            .is_none());
        assert_eq!(
            open_engine_for_choice(EngineChoice::Auto, "macos", &c).backend_kind(),
            Some(BackendKind::Mlx)
        );
        assert_eq!(
            open_engine_for_choice(EngineChoice::Auto, "linux", &c).backend_kind(),
            Some(BackendKind::LlamaPgrn)
        );
    }

    #[test]
    fn resolve_engine_choice_cli_wins() {
        let c = resolve_engine_choice(Some(EngineChoice::Mlx), true).unwrap();
        assert_eq!(c, EngineChoice::Mlx);
        let mock = resolve_engine_choice(None, true).unwrap();
        assert_eq!(mock, EngineChoice::Mock);
    }

    #[test]
    fn plan_serve_for_choice_mock_errors() {
        let err = plan_serve_for_choice(EngineChoice::Mock, "macos").unwrap_err();
        assert!(err.contains("mock"), "{err}");
    }

    #[test]
    fn plan_serve_llama_with_temp_bins() {
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
        // Avoid process-wide env (parallel tests); build plan via engine fields.
        let eng = LlamaPgrnEngine {
            server_path: bin.display().to_string(),
            model_path: model,
            ..LlamaPgrnEngine::from_env()
        };
        let plan = eng.plan_serve_argv().unwrap();
        assert_eq!(plan.backend, BackendKind::LlamaPgrn);
        assert!(plan.argv().iter().any(|a| a == "-m"));
        assert_eq!(
            EngineChoice::Llama.to_backend("linux"),
            Some(BackendKind::LlamaPgrn)
        );
    }

    #[test]
    fn launch_feature_default_off() {
        // Default features: launch is off in CI.
        #[cfg(not(feature = "launch"))]
        assert!(!launch_feature_enabled());
        #[cfg(feature = "launch")]
        assert!(launch_feature_enabled());
    }
}
