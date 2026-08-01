//! Stub pricing: credits charged / earned per completed job.

/// Default stub price: 1 credit per 1 000 tokens (ceil).
pub const DEFAULT_PRICE_CREDITS_PER_1K: u64 = 1;

/// Credits for a completed job: `ceil(tokens / 1000) * price_per_1k`.
///
/// Returns `0` when `tokens == 0`. When `tokens > 0`, the result is at least
/// `price_per_1k` (one partial 1k unit), or `1` if `price_per_1k == 0` is not
/// used — callers should pass a positive price.
pub fn credits_for_tokens(tokens: u64, price_credits_per_1k: u64) -> u64 {
    if tokens == 0 || price_credits_per_1k == 0 {
        return 0;
    }
    let units = tokens.div_ceil(1000);
    units.saturating_mul(price_credits_per_1k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_tokens_zero_credits() {
        assert_eq!(credits_for_tokens(0, 1), 0);
        assert_eq!(credits_for_tokens(0, 10), 0);
    }

    #[test]
    fn rounds_up_partial_thousand() {
        assert_eq!(credits_for_tokens(1, 1), 1);
        assert_eq!(credits_for_tokens(999, 1), 1);
        assert_eq!(credits_for_tokens(1000, 1), 1);
        assert_eq!(credits_for_tokens(1001, 1), 2);
        assert_eq!(credits_for_tokens(2500, 2), 6);
    }

    #[test]
    fn zero_price_yields_zero() {
        assert_eq!(credits_for_tokens(5000, 0), 0);
    }
}
