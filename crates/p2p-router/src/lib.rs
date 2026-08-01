//! World-class peer selection for Slipstream P2P inference.
//!
//! Prefer local Slipstream when capable; otherwise score remotes by hardware fit,
//! backend alignment ([`p2p_core::select_backend`]: MLX on Mac / llama elsewhere),
//! tok/s, RTT, price, reputation, and load. Coding contexts apply a sticky-session bonus.
//!
//! See [`score`] for the full scoring formula.

#![deny(missing_docs)]

pub mod score;
pub mod select;
pub mod types;

pub use score::{
    load_penalty, score_peer, BACKEND_OK, BACKEND_PREFERRED, LOAD_LINEAR, LOAD_QUAD, LOCAL_BONUS,
    STICKY_CODING_BONUS,
};
pub use select::{choose_route, rank_peers};
pub use types::{JobContext, PeerSnapshot, RouteDecision, RouteRequest, ScoreBreakdown};

// Re-export the core types this crate consumes so callers have one import path.
pub use p2p_core::{
    meets_min_hardware, select_backend, BackendKind, Capability, JobRequest, NodeId, MIN_RAM_GIB,
    MIN_VRAM_GIB,
};

#[cfg(test)]
mod property_tests {
    use super::*;
    use crate::types::test_util::{job, nid};
    use proptest::prelude::*;

    fn arb_backend() -> impl Strategy<Value = BackendKind> {
        prop_oneof![Just(BackendKind::Mlx), Just(BackendKind::LlamaPgrn)]
    }

    fn arb_peer() -> impl Strategy<Value = PeerSnapshot> {
        // Nested tuples keep us under proptest's 12-ary Strategy limit.
        (
            (any::<u64>(), any::<bool>(), 0u32..=128, 0u32..=64, arb_backend()),
            (
                prop_oneof![Just("macos"), Just("linux"), Just("windows")],
                0.0f64..120.0,
                0u32..5_000,
                0u64..50,
                0u32..=100,
                0.0f64..=1.05,
                any::<bool>(),
            ),
        )
            .prop_map(
                |((id_seed, local, ram, vram, backend), (os, tok_s, rtt, price, rep, load, has_model))| {
                    let mut id_bytes = [0u8; 32];
                    id_bytes[..8].copy_from_slice(&id_seed.to_le_bytes());
                    id_bytes[8] = ram as u8;
                    id_bytes[9] = vram as u8;
                    let mut capability = p2p_core::local_capability(
                        os,
                        ram,
                        vram,
                        if has_model {
                            vec!["m".into()]
                        } else {
                            vec!["other".into()]
                        },
                    );
                    capability.backend = backend;
                    capability.tok_s_estimate = tok_s;
                    capability.price_credits_per_1k = price;
                    PeerSnapshot {
                        node_id: NodeId::from_bytes(&id_bytes),
                        listen_addr: format!("127.0.0.1:{}", (id_seed % 50_000) + 1_024),
                        capability,
                        rtt_ms: rtt,
                        reputation: rep,
                        load,
                        is_local: local,
                    }
                },
            )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Property: choose_route never returns a peer that fails the hardware gate
        /// or is load-saturated / missing the model.
        #[test]
        fn never_selects_under_gated_peer(peers in prop::collection::vec(arb_peer(), 0..12)) {
            let req = RouteRequest::from_job(job("m"), JobContext::General, None, "linux");
            if let Some(decision) = choose_route(&peers, &req) {
                let selected = peers
                    .iter()
                    .find(|p| {
                        p.node_id == decision.node_id && p.listen_addr == decision.listen_addr
                    })
                    .expect("selected peer must be in input set");
                // Decision must itself re-score (gates held at selection time).
                assert!(
                    score_peer(selected, &req).is_some(),
                    "selected peer fails gates: ram={} vram={} load={} models={:?}",
                    selected.capability.ram_gib,
                    selected.capability.vram_gib,
                    selected.load,
                    selected.capability.models
                );
                assert!(meets_min_hardware(
                    selected.capability.ram_gib,
                    selected.capability.vram_gib
                ));
                assert!(selected.capability.supports_model("m"));
                assert!(selected.load < 1.0);
            }
        }

        /// Property: among two eligible peers that differ only in tok/s, higher wins.
        #[test]
        fn prefers_better_speed_fit(
            ram in 32u32..=128,
            base_tok in 5.0f64..40.0,
            delta in 1.0f64..30.0,
        ) {
            let mk = |label: &str, tok: f64| {
                let mut capability =
                    p2p_core::local_capability("linux", ram, 0, vec!["m".into()]);
                capability.backend = BackendKind::LlamaPgrn;
                capability.tok_s_estimate = tok;
                PeerSnapshot {
                    node_id: nid(label),
                    listen_addr: label.into(),
                    capability,
                    rtt_ms: 20,
                    reputation: 50,
                    load: 0.1,
                    is_local: false,
                }
            };
            let slow = mk("slow", base_tok);
            let fast = mk("fast", base_tok + delta);
            let req = RouteRequest::from_job(job("m"), JobContext::General, None, "linux");
            let d = choose_route(&[slow, fast], &req).unwrap();
            assert_eq!(d.node_id, nid("fast"));
        }
    }

    #[test]
    fn never_selects_explicit_under_gated_fixture() {
        let weak = {
            let mut capability = p2p_core::local_capability("linux", 16, 8, vec!["m".into()]);
            capability.backend = BackendKind::LlamaPgrn;
            capability.tok_s_estimate = 99.0;
            capability.price_credits_per_1k = 0;
            PeerSnapshot {
                node_id: nid("weak"),
                listen_addr: "w:1".into(),
                capability,
                rtt_ms: 1,
                reputation: 100,
                load: 0.0,
                is_local: false,
            }
        };
        let ok = {
            let mut capability = p2p_core::local_capability("linux", 32, 0, vec!["m".into()]);
            capability.backend = BackendKind::LlamaPgrn;
            capability.tok_s_estimate = 10.0;
            capability.price_credits_per_1k = 5;
            PeerSnapshot {
                node_id: nid("ok"),
                listen_addr: "o:1".into(),
                capability,
                rtt_ms: 50,
                reputation: 40,
                load: 0.3,
                is_local: false,
            }
        };
        let req = RouteRequest::from_job(job("m"), JobContext::General, None, "linux");
        let d = choose_route(&[weak.clone(), ok], &req).unwrap();
        assert_eq!(d.node_id, nid("ok"));
        assert!(choose_route(&[weak], &req).is_none());
    }
}
