//! Peer selection: prefer local Slipstream, else highest score.

use crate::score::score_peer;
use crate::types::{PeerSnapshot, RouteDecision, RouteRequest};

/// Choose the best peer for `req`.
///
/// Policy:
/// 1. Prefer local Slipstream when it passes gates (via [`crate::score::LOCAL_BONUS`]).
/// 2. Otherwise pick the maximum scored remote peer.
/// 3. Return `None` if no peer passes hard gates.
pub fn choose_route(peers: &[PeerSnapshot], req: &RouteRequest) -> Option<RouteDecision> {
    let mut best: Option<RouteDecision> = None;

    for peer in peers {
        let Some((score, breakdown)) = score_peer(peer, req) else {
            continue;
        };
        let decision = RouteDecision {
            node_id: peer.node_id.clone(),
            listen_addr: peer.listen_addr.clone(),
            is_local: peer.is_local,
            score,
            breakdown,
        };
        match &best {
            Some(cur) if cur.score >= decision.score => {}
            _ => best = Some(decision),
        }
    }

    best
}

/// Rank all eligible peers (highest score first). Useful for failover.
pub fn rank_peers(peers: &[PeerSnapshot], req: &RouteRequest) -> Vec<RouteDecision> {
    let mut ranked: Vec<RouteDecision> = peers
        .iter()
        .filter_map(|peer| {
            let (score, breakdown) = score_peer(peer, req)?;
            Some(RouteDecision {
                node_id: peer.node_id.clone(),
                listen_addr: peer.listen_addr.clone(),
                is_local: peer.is_local,
                score,
                breakdown,
            })
        })
        .collect();
    ranked.sort_by(|a, b| b.score.cmp(&a.score));
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::test_util::{job, nid, peer};
    use crate::types::{JobContext, RouteRequest};
    use p2p_core::BackendKind;

    #[test]
    fn prefers_local_slipstream_when_capable() {
        let local = peer(
            "local",
            true,
            36,
            0,
            BackendKind::Mlx,
            "macos",
            12.0,
            0,
            1,
            60,
            0.2,
            &["m"],
        );
        let remote = peer(
            "remote",
            false,
            128,
            48,
            BackendKind::LlamaPgrn,
            "linux",
            80.0,
            5,
            1,
            60,
            0.0,
            &["m"],
        );
        let req = RouteRequest::from_job(job("m"), JobContext::General, None, "macos");
        let d = choose_route(&[remote, local], &req).unwrap();
        assert!(d.is_local);
        assert_eq!(d.node_id, nid("local"));
    }

    #[test]
    fn skips_local_when_incapable_picks_best_remote() {
        let local = peer(
            "local",
            true,
            36,
            0,
            BackendKind::Mlx,
            "macos",
            12.0,
            0,
            1,
            60,
            0.1,
            &["other"],
        );
        let slow = peer(
            "slow",
            false,
            32,
            0,
            BackendKind::LlamaPgrn,
            "linux",
            8.0,
            300,
            5,
            60,
            0.4,
            &["m"],
        );
        let fast = peer(
            "fast",
            false,
            64,
            24,
            BackendKind::LlamaPgrn,
            "linux",
            45.0,
            15,
            1,
            60,
            0.1,
            &["m"],
        );
        let req = RouteRequest::from_job(job("m"), JobContext::General, None, "macos");
        let d = choose_route(&[local, slow, fast], &req).unwrap();
        assert!(!d.is_local);
        assert_eq!(d.node_id, nid("fast"));
    }

    #[test]
    fn sticky_coding_prefers_prior_peer() {
        let a = peer(
            "peer-a",
            false,
            64,
            0,
            BackendKind::LlamaPgrn,
            "linux",
            40.0,
            20,
            1,
            60,
            0.2,
            &["m"],
        );
        let mut b = peer(
            "peer-b",
            false,
            64,
            0,
            BackendKind::LlamaPgrn,
            "linux",
            42.0,
            18,
            1,
            60,
            0.2,
            &["m"],
        );
        b.capability.tok_s_estimate = 55.0;

        let sticky_req = RouteRequest::from_job(
            job("m"),
            JobContext::Coding,
            Some(nid("peer-a")),
            "linux",
        );
        let d = choose_route(&[a.clone(), b.clone()], &sticky_req).unwrap();
        assert_eq!(d.node_id, nid("peer-a"));
        assert!(d.breakdown.sticky_bonus > 0);

        let general =
            RouteRequest::from_job(job("m"), JobContext::General, Some(nid("peer-a")), "linux");
        let d2 = choose_route(&[a, b], &general).unwrap();
        assert_eq!(d2.node_id, nid("peer-b"));
    }

    #[test]
    fn prefers_mlx_remote_for_mac_requester() {
        let llama = peer(
            "llama",
            false,
            64,
            0,
            BackendKind::LlamaPgrn,
            "linux",
            30.0,
            30,
            1,
            60,
            0.1,
            &["m"],
        );
        let mlx = peer(
            "mlx",
            false,
            64,
            0,
            BackendKind::Mlx,
            "macos",
            30.0,
            30,
            1,
            60,
            0.1,
            &["m"],
        );
        let req = RouteRequest::from_job(job("m"), JobContext::General, None, "macos");
        let d = choose_route(&[llama, mlx], &req).unwrap();
        assert_eq!(d.node_id, nid("mlx"));
    }
}
