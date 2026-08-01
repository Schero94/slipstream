//! Library surface for `p2p-node` (CLI binary + integration tests).

pub mod runtime;
pub mod wire;

pub use p2p_engine::{
    launch_feature_enabled, plan_serve_for_choice, EngineChoice, ServePlan,
};
pub use runtime::{
    capability_for_engine, capability_to_advert, client_hello, default_capability, send_sealed_job,
    NodeConfig, RunningNode, RuntimeError,
};
pub use wire::{net_to_sealed, sealed_to_net, WireError};
