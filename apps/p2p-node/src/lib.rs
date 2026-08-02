//! Library surface for `p2p-node` (CLI binary + integration tests).

pub mod config;
pub mod runtime;
pub mod spawn_guard;
pub mod wire;

pub use config::{NodeMode, NodePolicy, PolicyError};
pub use p2p_engine::{launch_feature_enabled, plan_serve_for_choice, EngineChoice, ServePlan};
pub use runtime::{
    capability_for_engine, capability_to_advert, client_hello, default_capability, send_sealed_job,
    NodeConfig, RunningNode, RuntimeError,
};
pub use spawn_guard::{
    check_spawn_engine_safe, check_spawn_engine_safe_with, default_lock_path,
    resolve_guard_endpoint, REFUSE_PREFIX,
};
pub use wire::{net_to_sealed, sealed_result_to_net, sealed_to_net, WireError};
