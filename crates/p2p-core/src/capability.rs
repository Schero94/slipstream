//! Capability advertisement and hardware / backend selection.

use serde::{Deserialize, Serialize};

/// Minimum system RAM (GiB) to earn as a provider when VRAM is below threshold.
pub const MIN_RAM_GIB: u32 = 32;
/// Minimum VRAM (GiB) alternative to the RAM gate.
pub const MIN_VRAM_GIB: u32 = 16;

/// Production inference backend family (OS-selected).
///
/// Decision: macOS/Darwin/iOS → [`BackendKind::Mlx`] (oMLX / MLX PGRN);
/// everything else → [`BackendKind::LlamaPgrn`] (llama.cpp fork).
/// Mock inference is an [`crate::engine::InferenceEngine`] implementation, not a backend kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// macOS oMLX / MLX PGRN path.
    Mlx,
    /// Non-Mac llama.cpp / PGRN Metal-or-CUDA path.
    LlamaPgrn,
}

/// Advertised node capabilities for routing (`p2p-router` consumes this).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capability {
    pub os: String,
    pub ram_gib: u32,
    pub vram_gib: u32,
    pub backend: BackendKind,
    pub models: Vec<String>,
    pub max_ctx: u32,
    pub tok_s_estimate: f64,
    pub price_credits_per_1k: u64,
}

impl Capability {
    pub fn meets_min_hardware(&self) -> bool {
        meets_min_hardware(self.ram_gib, self.vram_gib)
    }

    pub fn supports_model(&self, model: &str) -> bool {
        self.models.iter().any(|m| m == model)
    }
}

/// True if the node may earn: ≥32 GiB RAM **or** ≥16 GiB VRAM.
pub fn meets_min_hardware(ram_gib: u32, vram_gib: u32) -> bool {
    ram_gib >= MIN_RAM_GIB || vram_gib >= MIN_VRAM_GIB
}

/// Select the recommended production backend for an OS string.
pub fn select_backend(os: &str) -> BackendKind {
    let os = os.to_ascii_lowercase();
    if os.contains("macos") || os.contains("darwin") || os.contains("ios") {
        BackendKind::Mlx
    } else {
        BackendKind::LlamaPgrn
    }
}

/// Build a default capability advert for a local node.
pub fn local_capability(os: &str, ram_gib: u32, vram_gib: u32, models: Vec<String>) -> Capability {
    Capability {
        os: os.to_string(),
        ram_gib,
        vram_gib,
        backend: select_backend(os),
        models,
        max_ctx: 8192,
        tok_s_estimate: 12.0,
        price_credits_per_1k: 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_hardware_ram_or_vram() {
        assert!(meets_min_hardware(32, 0));
        assert!(meets_min_hardware(8, 16));
        assert!(!meets_min_hardware(31, 15));
        assert!(meets_min_hardware(64, 4));
    }

    #[test]
    fn select_backend_macos_is_mlx() {
        assert_eq!(select_backend("macos"), BackendKind::Mlx);
        assert_eq!(select_backend("Darwin"), BackendKind::Mlx);
        assert_eq!(select_backend("ios"), BackendKind::Mlx);
    }

    #[test]
    fn select_backend_linux_windows_is_llama() {
        assert_eq!(select_backend("linux"), BackendKind::LlamaPgrn);
        assert_eq!(select_backend("windows"), BackendKind::LlamaPgrn);
    }

    #[test]
    fn capability_serde_and_helpers() {
        let cap = local_capability("macos", 36, 0, vec!["qwen3-30b".into()]);
        assert_eq!(cap.backend, BackendKind::Mlx);
        assert!(cap.meets_min_hardware());
        assert!(cap.supports_model("qwen3-30b"));
        let json = serde_json::to_string(&cap).unwrap();
        let back: Capability = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cap);
    }
}
