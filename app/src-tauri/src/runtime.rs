use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};

const MANIFEST_NAME: &str = "runtime-manifest.json";

#[derive(Debug, Clone, Deserialize)]
struct RuntimeManifest {
    schema: u32,
    product_engine: String,
    ollama: bool,
    components: BTreeMap<String, ComponentSpec>,
    mlx_packages: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ComponentSpec {
    path: String,
    kind: String,
    required: bool,
    executable: bool,
    platform: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComponentReport {
    pub name: String,
    pub path: String,
    pub applicable: bool,
    pub required: bool,
    pub ready: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeReport {
    pub schema: u32,
    pub product_engine: String,
    pub ollama: bool,
    pub platform: String,
    pub ready: bool,
    pub components: Vec<ComponentReport>,
    pub mlx_packages: BTreeMap<String, String>,
}

pub fn current_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
        .replace("macos-aarch64", "macos-arm64")
        .replace("linux-aarch64", "linux-arm64")
}

fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

fn is_executable(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        true
    }
}

pub fn preflight(resource_root: &Path) -> Result<RuntimeReport, String> {
    let raw = fs::read(resource_root.join(MANIFEST_NAME))
        .map_err(|e| format!("runtime manifest read failed: {e}"))?;
    let manifest: RuntimeManifest = serde_json::from_slice(&raw)
        .map_err(|e| format!("runtime manifest parse failed: {e}"))?;
    if manifest.schema != 1 {
        return Err(format!(
            "unsupported runtime manifest schema {} (expected 1)",
            manifest.schema
        ));
    }
    if manifest.ollama || manifest.product_engine != "llama.cpp-pgrn" {
        return Err("runtime manifest must declare native llama.cpp-pgrn without Ollama".into());
    }

    let platform = current_platform();
    let canonical_root = resource_root
        .canonicalize()
        .map_err(|e| format!("runtime resource root is unavailable: {e}"))?;
    let mut ready = true;
    let mut components = Vec::with_capacity(manifest.components.len());

    for (name, spec) in manifest.components {
        let applicable = spec.platform.as_deref().is_none_or(|p| p == platform);
        if !applicable {
            components.push(ComponentReport {
                name,
                path: spec.path,
                applicable: false,
                required: spec.required,
                ready: true,
                detail: "not applicable on this platform".into(),
            });
            continue;
        }

        let relative = Path::new(&spec.path);
        let mut component_ready = false;
        let detail = if !safe_relative(relative) {
            "unsafe relative path".into()
        } else {
            let candidate = resource_root.join(relative);
            match fs::metadata(&candidate) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    format!(
                        "missing {} {}",
                        if spec.required { "required" } else { "optional" },
                        spec.kind
                    )
                }
                Err(e) => format!("metadata failed: {e}"),
                Ok(metadata) => {
                    let type_ok = match spec.kind.as_str() {
                        "file" => metadata.is_file(),
                        "directory" => metadata.is_dir(),
                        _ => false,
                    };
                    if !matches!(spec.kind.as_str(), "file" | "directory") {
                        format!("unsupported component kind {}", spec.kind)
                    } else if !type_ok {
                        format!("expected regular {}", spec.kind)
                    } else {
                        match candidate.canonicalize() {
                            Err(e) => format!("canonicalize failed: {e}"),
                            Ok(resolved) if !resolved.starts_with(&canonical_root) => {
                                "resolved path escapes resource root".into()
                            }
                            Ok(_) if spec.executable && !is_executable(&metadata) => {
                                "not executable".into()
                            }
                            Ok(_) => {
                                component_ready = true;
                                "ready".into()
                            }
                        }
                    }
                }
            }
        };

        if spec.required && !component_ready {
            ready = false;
        }
        components.push(ComponentReport {
            name,
            path: spec.path,
            applicable: true,
            required: spec.required,
            ready: component_ready,
            detail,
        });
    }

    Ok(RuntimeReport {
        schema: manifest.schema,
        product_engine: manifest.product_engine,
        ollama: manifest.ollama,
        platform,
        ready,
        components,
        mlx_packages: manifest.mlx_packages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "slipstream-runtime-{label}-{}-{stamp}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write_manifest(&self, components: serde_json::Value) {
            let value = json!({
                "schema": 1,
                "product_engine": "llama.cpp-pgrn",
                "ollama": false,
                "components": components,
                "mlx_packages": {"mlx": "0.32.0"}
            });
            fs::write(
                self.0.join(MANIFEST_NAME),
                serde_json::to_vec_pretty(&value).unwrap(),
            )
            .unwrap();
        }

        fn write_executable(&self, relative: &str) {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
            #[cfg(unix)]
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn component(path: &str, required: bool) -> serde_json::Value {
        json!({
            "path": path,
            "kind": "file",
            "required": required,
            "executable": true
        })
    }

    #[test]
    fn rejects_absent_required_component() {
        let root = TempRoot::new("missing");
        root.write_manifest(json!({"server": component("llama-server", true)}));
        let report = preflight(root.path()).unwrap();
        assert!(!report.ready);
        assert_eq!(report.components[0].detail, "missing required file");
    }

    #[test]
    fn rejects_parent_traversal() {
        let root = TempRoot::new("traversal");
        root.write_manifest(json!({"server": component("../escape", true)}));
        let report = preflight(root.path()).unwrap();
        assert!(!report.ready);
        assert!(report.components[0].detail.contains("unsafe relative path"));
    }

    #[test]
    fn skips_platform_mismatch_without_failing_required_gate() {
        let root = TempRoot::new("platform");
        let mut spec = component("omlx", true);
        spec["platform"] = json!("definitely-not-this-platform");
        root.write_manifest(json!({"omlx": spec}));
        let report = preflight(root.path()).unwrap();
        assert!(report.ready);
        assert!(!report.components[0].applicable);
        assert_eq!(report.components[0].detail, "not applicable on this platform");
    }

    #[test]
    fn requires_executable_regular_file() {
        let root = TempRoot::new("exec");
        root.write_manifest(json!({"server": component("llama-server", true)}));
        fs::create_dir(root.path().join("llama-server")).unwrap();
        let report = preflight(root.path()).unwrap();
        assert!(!report.ready);
        assert_eq!(report.components[0].detail, "expected regular file");
    }

    #[test]
    fn accepts_ready_executable() {
        let root = TempRoot::new("ready");
        root.write_manifest(json!({"server": component("llama-server", true)}));
        root.write_executable("llama-server");
        let report = preflight(root.path()).unwrap();
        assert!(report.ready);
        assert!(report.components[0].ready);
    }

    #[test]
    fn returns_parse_error_without_panicking() {
        let root = TempRoot::new("json");
        fs::write(root.path().join(MANIFEST_NAME), b"{not-json").unwrap();
        let error = preflight(root.path()).unwrap_err();
        assert!(error.contains("runtime manifest"));
    }
}
