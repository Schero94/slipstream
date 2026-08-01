//! Peer scoring formula for inference P2P.
//!
//! # Formula (higher is better)
//!
//! Hard gates (peer rejected → `None`):
//! 1. [`p2p_core::meets_min_hardware`] — `ram_gib >= 32 OR vram_gib >= 16`
//! 2. peer advertises the requested model ([`JobRequest::model`])
//! 3. `load < 1.0` (fully saturated peers are never selected)
//!
//! Soft score:
//! ```text
//! score =
//!     local_bonus          // +100_000 if local Slipstream (hard prefer when capable)
//!   + backend_fit          // prefer select_backend(requester_os); Mlx-on-Mac > Llama elsewhere
//!   + hardware_fit         // headroom above min gate + ctx capacity
//!   + speed                // tok_s_estimate × 20
//!   + reputation           // reputation[0..=100] × 15
//!   + sticky_bonus         // +40_000 when coding context + sticky peer match
//!   − latency_penalty      // rtt_ms / 2  (capped)
//!   − price_penalty        // price_credits_per_1k × 25 (capped)
//!   − load_penalty         // load × LOAD_LINEAR + load² × LOAD_QUAD
//! ```
//!
//! ## Load penalty (swarm opt #1)
//!
//! Hot peers must shed work earlier than a pure linear term allows. The soft
//! penalty is therefore linear + quadratic in `load ∈ [0, 1]`:
//!
//! ```text
//! load_penalty = load × 12_000 + load² × 16_000
//! ```
//!
//! At `load ≈ 0.99` this peaks near ~27.5k — below sticky (+40k) and local
//! (+100k), so coding stickiness and local Slipstream still dominate. Among
//! near-ties, the quadratic term tips selection toward idle seeders under
//! saturation (SWARM_BENCH finding #1).

use p2p_core::{select_backend, BackendKind, Capability, MIN_RAM_GIB, MIN_VRAM_GIB};

use crate::types::{JobContext, PeerSnapshot, RouteRequest, ScoreBreakdown};

/// Local Slipstream gets an overwhelming bonus so capable local always wins.
pub const LOCAL_BONUS: i64 = 100_000;
/// Sticky coding session stickiness.
pub const STICKY_CODING_BONUS: i64 = 40_000;
/// Preferred backend match (OS-aligned via [`select_backend`]).
pub const BACKEND_PREFERRED: i64 = 8_000;
/// Acceptable but non-preferred backend.
pub const BACKEND_OK: i64 = 3_000;
/// Linear load weight in `load × LOAD_LINEAR + load² × LOAD_QUAD`.
pub const LOAD_LINEAR: i64 = 12_000;
/// Quadratic load weight — sheds hot peers earlier under saturation.
pub const LOAD_QUAD: i64 = 16_000;

/// Score a peer for `req`. Returns `None` when a hard gate fails.
pub fn score_peer(peer: &PeerSnapshot, req: &RouteRequest) -> Option<(i64, ScoreBreakdown)> {
    if !peer.capability.meets_min_hardware() {
        return None;
    }
    if !peer.capability.supports_model(req.model()) {
        return None;
    }
    if !(peer.load < 1.0) {
        return None;
    }

    let mut b = ScoreBreakdown::default();

    if peer.is_local {
        b.local_bonus = LOCAL_BONUS;
    }

    b.backend_fit = backend_fit(&peer.capability, &req.requester_os);
    b.hardware_fit = hardware_fit(&peer.capability);
    b.speed = (peer.capability.tok_s_estimate.max(0.0) * 20.0) as i64;
    b.reputation = i64::from(peer.reputation.min(100)) * 15;
    b.latency_penalty = i64::from(peer.rtt_ms.min(10_000)) / 2;
    b.price_penalty =
        i64::try_from(peer.capability.price_credits_per_1k.min(200)).unwrap_or(200) * 25;
    b.load_penalty = load_penalty(peer.load);

    if req.context == JobContext::Coding {
        if let Some(sticky) = &req.sticky_peer {
            if sticky == &peer.node_id {
                b.sticky_bonus = STICKY_CODING_BONUS;
            }
        }
    }

    Some((b.total(), b))
}

/// Soft load penalty: `load × LOAD_LINEAR + load² × LOAD_QUAD` (clamped to `[0, 1]`).
pub fn load_penalty(load: f64) -> i64 {
    let l = load.clamp(0.0, 1.0);
    let linear = l * LOAD_LINEAR as f64;
    let quad = l * l * LOAD_QUAD as f64;
    (linear + quad) as i64
}

fn backend_fit(cap: &Capability, requester_os: &str) -> i64 {
    let preferred = select_backend(requester_os);
    match (preferred, cap.backend) {
        (BackendKind::Mlx, BackendKind::Mlx) => BACKEND_PREFERRED,
        (BackendKind::LlamaPgrn, BackendKind::LlamaPgrn) => BACKEND_PREFERRED,
        // Remote Mac peer advertising Mlx remains a strong remote choice even for
        // non-Mac requesters (local > mlx-on-mac > llama elsewhere).
        (_, BackendKind::Mlx) => BACKEND_PREFERRED - 500,
        (_, BackendKind::LlamaPgrn) => BACKEND_OK,
    }
}

/// Hardware headroom: reward excess RAM/VRAM and usable context.
fn hardware_fit(cap: &Capability) -> i64 {
    let ram_headroom = i64::from(cap.ram_gib.saturating_sub(MIN_RAM_GIB)).min(64) * 40;
    let vram_headroom = i64::from(cap.vram_gib.saturating_sub(MIN_VRAM_GIB)).min(48) * 60;
    let headroom = ram_headroom.max(vram_headroom).max(0);
    let ctx_bonus = i64::from(cap.max_ctx.min(128_000) / 1_024);
    headroom + ctx_bonus
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::test_util::{job, nid, peer};
    use crate::types::{JobContext, RouteRequest};
    use p2p_core::BackendKind;

    #[test]
    fn gate_rejects_under_min_hardware() {
        let p = peer(
            "weak",
            false,
            16,
            8,
            BackendKind::LlamaPgrn,
            "linux",
            30.0,
            10,
            1,
            80,
            0.1,
            &["m"],
        );
        let req = RouteRequest::from_job(job("m"), JobContext::General, None, "linux");
        assert!(score_peer(&p, &req).is_none());
        assert!(!p2p_core::meets_min_hardware(16, 8));
    }

    #[test]
    fn gate_accepts_vram_alternative() {
        let p = peer(
            "gpu",
            false,
            16,
            16,
            BackendKind::LlamaPgrn,
            "linux",
            40.0,
            10,
            1,
            50,
            0.2,
            &["m"],
        );
        let req = RouteRequest::from_job(job("m"), JobContext::General, None, "linux");
        assert!(score_peer(&p, &req).is_some());
    }

    #[test]
    fn saturated_load_rejected() {
        let p = peer(
            "busy",
            false,
            64,
            0,
            BackendKind::LlamaPgrn,
            "linux",
            40.0,
            5,
            1,
            90,
            1.0,
            &["m"],
        );
        let req = RouteRequest::from_job(job("m"), JobContext::General, None, "linux");
        assert!(score_peer(&p, &req).is_none());
    }

    #[test]
    fn faster_cheaper_peer_scores_higher() {
        let slow = peer(
            "slow",
            false,
            32,
            0,
            BackendKind::LlamaPgrn,
            "linux",
            10.0,
            200,
            10,
            50,
            0.5,
            &["m"],
        );
        let fast = peer(
            "fast",
            false,
            64,
            24,
            BackendKind::LlamaPgrn,
            "linux",
            50.0,
            20,
            1,
            50,
            0.1,
            &["m"],
        );
        let req = RouteRequest::from_job(job("m"), JobContext::General, None, "linux");
        let (s_slow, _) = score_peer(&slow, &req).unwrap();
        let (s_fast, _) = score_peer(&fast, &req).unwrap();
        assert!(s_fast > s_slow, "fast={s_fast} slow={s_slow}");
    }

    #[test]
    fn backend_fit_uses_select_backend() {
        assert_eq!(select_backend("macos"), BackendKind::Mlx);
        assert_eq!(select_backend("linux"), BackendKind::LlamaPgrn);
        let mlx = peer(
            "mlx",
            false,
            64,
            0,
            BackendKind::Mlx,
            "macos",
            30.0,
            20,
            1,
            50,
            0.1,
            &["m"],
        );
        let llama = peer(
            "llama",
            false,
            64,
            0,
            BackendKind::LlamaPgrn,
            "linux",
            30.0,
            20,
            1,
            50,
            0.1,
            &["m"],
        );
        let req = RouteRequest::from_job(job("m"), JobContext::General, None, "macos");
        let (s_mlx, b_mlx) = score_peer(&mlx, &req).unwrap();
        let (s_llama, b_llama) = score_peer(&llama, &req).unwrap();
        assert!(b_mlx.backend_fit > b_llama.backend_fit);
        assert!(s_mlx > s_llama);
    }

    #[test]
    fn lower_load_wins_when_other_factors_equal() {
        let idle = peer(
            "idle",
            false,
            64,
            0,
            BackendKind::LlamaPgrn,
            "linux",
            40.0,
            20,
            1,
            60,
            0.1,
            &["m"],
        );
        let busy = peer(
            "busy",
            false,
            64,
            0,
            BackendKind::LlamaPgrn,
            "linux",
            40.0,
            20,
            1,
            60,
            0.7,
            &["m"],
        );
        let req = RouteRequest::from_job(job("m"), JobContext::General, None, "linux");
        let (s_idle, b_idle) = score_peer(&idle, &req).unwrap();
        let (s_busy, b_busy) = score_peer(&busy, &req).unwrap();
        assert!(
            b_busy.load_penalty > b_idle.load_penalty,
            "busy penalty {} should exceed idle {}",
            b_busy.load_penalty,
            b_idle.load_penalty
        );
        assert!(
            s_idle > s_busy,
            "idle={s_idle} should beat busy={s_busy} when only load differs"
        );
    }

    #[test]
    fn load_penalty_formula_is_linear_plus_quadratic() {
        // load=0.5 → 0.5×12000 + 0.25×16000 = 6000 + 4000 = 10000
        assert_eq!(load_penalty(0.5), 10_000);
        // load=0 → 0
        assert_eq!(load_penalty(0.0), 0);
        // Quadratic grows faster toward saturation than pure linear.
        let mid = load_penalty(0.5);
        let hot = load_penalty(0.9);
        assert!(hot > mid * 2, "hot={hot} should more than double mid={mid}");
    }

    #[test]
    fn load_penalty_stays_below_sticky_and_local() {
        let peak = load_penalty(0.99);
        assert!(
            peak < STICKY_CODING_BONUS,
            "peak load penalty {peak} must stay under sticky {STICKY_CODING_BONUS}"
        );
        assert!(
            peak < LOCAL_BONUS,
            "peak load penalty {peak} must stay under local {LOCAL_BONUS}"
        );
    }

    #[test]
    fn sticky_coding_beats_hotter_idle_alternative() {
        // Sticky peer is hot; alternative is idle and slightly faster — sticky must win.
        let sticky = peer(
            "sticky",
            false,
            64,
            0,
            BackendKind::LlamaPgrn,
            "linux",
            40.0,
            20,
            1,
            60,
            0.9,
            &["m"],
        );
        let alt = peer(
            "alt",
            false,
            64,
            0,
            BackendKind::LlamaPgrn,
            "linux",
            55.0,
            18,
            1,
            60,
            0.0,
            &["m"],
        );
        let req = RouteRequest::from_job(
            job("m"),
            JobContext::Coding,
            Some(nid("sticky")),
            "linux",
        );
        let (s_sticky, b_sticky) = score_peer(&sticky, &req).unwrap();
        let (s_alt, _) = score_peer(&alt, &req).unwrap();
        assert_eq!(b_sticky.sticky_bonus, STICKY_CODING_BONUS);
        assert!(
            s_sticky > s_alt,
            "sticky={s_sticky} must beat idle alt={s_alt}"
        );
    }
}
