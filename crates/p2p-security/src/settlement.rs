//! Settlement honesty for stub credits (TM-005 freeload / underpay).
//!
//! Credit only when a completion receipt exists and amount ≥ quoted price.

use std::collections::HashMap;

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SettlementError {
    #[error("no completion receipt for job")]
    MissingReceipt,
    #[error("underpay: offered {offered} < quoted {quoted}")]
    Underpay { offered: u64, quoted: u64 },
    #[error("job already settled")]
    AlreadySettled,
}

/// Worker-side proof that a job finished (MVP: local struct, later signed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionReceipt {
    pub job_id: String,
    pub worker_id_hex: String,
    pub tokens: u32,
    pub quoted_credits: u64,
}

/// Guard that refuses freeload and underpay against a receipt table.
#[derive(Debug, Default)]
pub struct SettlementGuard {
    receipts: HashMap<String, CompletionReceipt>,
    settled: HashMap<String, u64>,
}

impl SettlementGuard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_completion(&mut self, receipt: CompletionReceipt) {
        self.receipts.insert(receipt.job_id.clone(), receipt);
    }

    /// Credits for a completed job: ceil(tokens/1000) * price_per_1k, min 1 if tokens > 0.
    pub fn quote(tokens: u32, price_credits_per_1k: u64) -> u64 {
        if tokens == 0 {
            return 0;
        }
        let units = ((tokens as u64) + 999) / 1000;
        (units * price_credits_per_1k).max(1)
    }

    /// Attempt to settle `offered` credits for `job_id`. Rejects freeload & underpay.
    pub fn settle(&mut self, job_id: &str, offered: u64) -> Result<u64, SettlementError> {
        if self.settled.contains_key(job_id) {
            return Err(SettlementError::AlreadySettled);
        }
        let receipt = self
            .receipts
            .get(job_id)
            .ok_or(SettlementError::MissingReceipt)?;
        let quoted = receipt.quoted_credits;
        if offered < quoted {
            return Err(SettlementError::Underpay { offered, quoted });
        }
        self.settled.insert(job_id.to_string(), offered);
        Ok(offered)
    }

    pub fn is_settled(&self, job_id: &str) -> bool {
        self.settled.contains_key(job_id)
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_zero_tokens_is_zero() {
        assert_eq!(SettlementGuard::quote(0, 10), 0);
    }

    #[test]
    fn quote_ceils_to_1k_units_with_floor_one() {
        assert_eq!(SettlementGuard::quote(1, 5), 5);
        assert_eq!(SettlementGuard::quote(1000, 5), 5);
        assert_eq!(SettlementGuard::quote(1001, 5), 10);
    }

    #[test]
    fn settle_exact_quote_marks_settled() {
        let mut g = SettlementGuard::new();
        g.record_completion(CompletionReceipt {
            job_id: "j1".into(),
            worker_id_hex: "aa".into(),
            tokens: 100,
            quoted_credits: 7,
        });
        assert_eq!(g.settle("j1", 7).unwrap(), 7);
        assert!(g.is_settled("j1"));
        assert_eq!(g.settle("j1", 7), Err(SettlementError::AlreadySettled));
    }
}
