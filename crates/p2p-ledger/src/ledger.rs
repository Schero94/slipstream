//! SQLite-backed credits ledger (file or in-memory).
//!
//! Settlement is atomic and idempotent on `job_id`: concurrent retries of the
//! same job never double-credit / double-debit.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::error::LedgerError;
use crate::pricing::{credits_for_tokens, DEFAULT_PRICE_CREDITS_PER_1K};

/// Outcome of [`Ledger::settle`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SettleOutcome {
    /// First successful settlement for this `job_id`.
    Settled {
        job_id: String,
        consumer_id: String,
        provider_id: String,
        tokens: u64,
        credits: u64,
    },
    /// Prior settlement reused; balances unchanged.
    AlreadySettled {
        job_id: String,
        consumer_id: String,
        provider_id: String,
        tokens: u64,
        credits: u64,
    },
}

impl SettleOutcome {
    pub fn credits(&self) -> u64 {
        match self {
            SettleOutcome::Settled { credits, .. }
            | SettleOutcome::AlreadySettled { credits, .. } => *credits,
        }
    }

    pub fn is_first(&self) -> bool {
        matches!(self, SettleOutcome::Settled { .. })
    }
}

/// Persistent record of a settled job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementRecord {
    pub job_id: String,
    pub consumer_id: String,
    pub provider_id: String,
    pub tokens: u64,
    pub credits: u64,
}

/// Thread-safe credits ledger.
///
/// Uses a single SQLite connection behind a mutex. Concurrent callers serialize
/// on the mutex; `BEGIN IMMEDIATE` plus a `UNIQUE(job_id)` constraint make
/// settlement idempotent under contention.
#[derive(Clone)]
pub struct Ledger {
    conn: Arc<Mutex<Connection>>,
    price_credits_per_1k: u64,
}

impl Ledger {
    /// In-memory ledger (shared cache so the connection sees its own writes).
    pub fn open_memory() -> Result<Self, LedgerError> {
        Self::open_memory_with_price(DEFAULT_PRICE_CREDITS_PER_1K)
    }

    pub fn open_memory_with_price(price_credits_per_1k: u64) -> Result<Self, LedgerError> {
        let conn = Connection::open_in_memory()?;
        // Keep a stable in-memory DB if we ever open additional handles.
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        let ledger = Self {
            conn: Arc::new(Mutex::new(conn)),
            price_credits_per_1k,
        };
        ledger.migrate()?;
        Ok(ledger)
    }

    /// File-backed SQLite ledger.
    pub fn open_sqlite(path: impl AsRef<Path>) -> Result<Self, LedgerError> {
        Self::open_sqlite_with_price(path, DEFAULT_PRICE_CREDITS_PER_1K)
    }

    pub fn open_sqlite_with_price(
        path: impl AsRef<Path>,
        price_credits_per_1k: u64,
    ) -> Result<Self, LedgerError> {
        let conn = Connection::open(path.as_ref())?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let ledger = Self {
            conn: Arc::new(Mutex::new(conn)),
            price_credits_per_1k,
        };
        ledger.migrate()?;
        Ok(ledger)
    }

    pub fn price_credits_per_1k(&self) -> u64 {
        self.price_credits_per_1k
    }

    fn migrate(&self) -> Result<(), LedgerError> {
        let conn = self.conn.lock().expect("ledger mutex");
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS accounts (
                id TEXT PRIMARY KEY NOT NULL,
                balance INTEGER NOT NULL DEFAULT 0 CHECK (balance >= 0)
            );

            CREATE TABLE IF NOT EXISTS settlements (
                job_id TEXT PRIMARY KEY NOT NULL,
                consumer_id TEXT NOT NULL,
                provider_id TEXT NOT NULL,
                tokens INTEGER NOT NULL CHECK (tokens >= 0),
                credits INTEGER NOT NULL CHECK (credits >= 0),
                settled_at_ms INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_settlements_consumer
                ON settlements(consumer_id);
            CREATE INDEX IF NOT EXISTS idx_settlements_provider
                ON settlements(provider_id);
            "#,
        )?;
        Ok(())
    }

    /// Grant credits to an account (free-tier faucet / bootstrap funding).
    pub fn fund(&self, account_id: &str, amount: u64) -> Result<u64, LedgerError> {
        if account_id.is_empty() {
            return Err(LedgerError::InvalidSettlement(
                "account_id must be non-empty".into(),
            ));
        }
        if amount == 0 {
            return self.balance(account_id);
        }
        let conn = self.conn.lock().expect("ledger mutex");
        let tx = conn.unchecked_transaction()?;
        ensure_account(&tx, account_id)?;
        tx.execute(
            "UPDATE accounts SET balance = balance + ?1 WHERE id = ?2",
            params![amount as i64, account_id],
        )?;
        let bal: i64 = tx.query_row(
            "SELECT balance FROM accounts WHERE id = ?1",
            params![account_id],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(bal as u64)
    }

    /// Current balance (0 if the account has never been funded/settled).
    pub fn balance(&self, account_id: &str) -> Result<u64, LedgerError> {
        let conn = self.conn.lock().expect("ledger mutex");
        let bal: Option<i64> = conn
            .query_row(
                "SELECT balance FROM accounts WHERE id = ?1",
                params![account_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(bal.unwrap_or(0) as u64)
    }

    /// Debit consumer and credit provider for a completed job.
    ///
    /// Idempotent on `job_id`. Credits = [`credits_for_tokens`] with this
    /// ledger's configured price.
    pub fn settle(
        &self,
        job_id: &str,
        consumer_id: &str,
        provider_id: &str,
        tokens: u64,
    ) -> Result<SettleOutcome, LedgerError> {
        validate_settle_args(job_id, consumer_id, provider_id)?;
        let credits = credits_for_tokens(tokens, self.price_credits_per_1k);

        let mut conn = self.conn.lock().expect("ledger mutex");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(existing) = load_settlement(&tx, job_id)? {
            return Ok(SettleOutcome::AlreadySettled {
                job_id: existing.job_id,
                consumer_id: existing.consumer_id,
                provider_id: existing.provider_id,
                tokens: existing.tokens,
                credits: existing.credits,
            });
        }

        // Zero-credit jobs still record settlement for idempotency.
        ensure_account(&tx, consumer_id)?;
        ensure_account(&tx, provider_id)?;

        if credits > 0 {
            let consumer_bal: i64 = tx.query_row(
                "SELECT balance FROM accounts WHERE id = ?1",
                params![consumer_id],
                |row| row.get(0),
            )?;
            if (consumer_bal as u64) < credits {
                return Err(LedgerError::InsufficientCredits {
                    account: consumer_id.to_string(),
                    need: credits,
                    have: consumer_bal as u64,
                });
            }
            tx.execute(
                "UPDATE accounts SET balance = balance - ?1 WHERE id = ?2",
                params![credits as i64, consumer_id],
            )?;
            tx.execute(
                "UPDATE accounts SET balance = balance + ?1 WHERE id = ?2",
                params![credits as i64, provider_id],
            )?;
        }

        let now_ms = now_unix_ms();
        tx.execute(
            r#"
            INSERT INTO settlements (job_id, consumer_id, provider_id, tokens, credits, settled_at_ms)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                job_id,
                consumer_id,
                provider_id,
                tokens as i64,
                credits as i64,
                now_ms
            ],
        )?;

        tx.commit()?;

        Ok(SettleOutcome::Settled {
            job_id: job_id.to_string(),
            consumer_id: consumer_id.to_string(),
            provider_id: provider_id.to_string(),
            tokens,
            credits,
        })
    }

    /// Look up a prior settlement by `job_id`.
    pub fn get_settlement(&self, job_id: &str) -> Result<Option<SettlementRecord>, LedgerError> {
        let conn = self.conn.lock().expect("ledger mutex");
        load_settlement(&conn, job_id)
    }
}

fn validate_settle_args(
    job_id: &str,
    consumer_id: &str,
    provider_id: &str,
) -> Result<(), LedgerError> {
    if job_id.is_empty() {
        return Err(LedgerError::InvalidSettlement(
            "job_id must be non-empty".into(),
        ));
    }
    if consumer_id.is_empty() || provider_id.is_empty() {
        return Err(LedgerError::InvalidSettlement(
            "consumer_id and provider_id must be non-empty".into(),
        ));
    }
    if consumer_id == provider_id {
        return Err(LedgerError::InvalidSettlement(
            "consumer and provider must differ".into(),
        ));
    }
    Ok(())
}

fn ensure_account(conn: &Connection, account_id: &str) -> Result<(), LedgerError> {
    conn.execute(
        "INSERT OR IGNORE INTO accounts (id, balance) VALUES (?1, 0)",
        params![account_id],
    )?;
    Ok(())
}

fn load_settlement(
    conn: &Connection,
    job_id: &str,
) -> Result<Option<SettlementRecord>, LedgerError> {
    let row = conn
        .query_row(
            r#"
            SELECT job_id, consumer_id, provider_id, tokens, credits
            FROM settlements WHERE job_id = ?1
            "#,
            params![job_id],
            |row| {
                Ok(SettlementRecord {
                    job_id: row.get(0)?,
                    consumer_id: row.get(1)?,
                    provider_id: row.get(2)?,
                    tokens: row.get::<_, i64>(3)? as u64,
                    credits: row.get::<_, i64>(4)? as u64,
                })
            },
        )
        .optional()?;
    Ok(row)
}

fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    #[test]
    fn fund_and_balance() {
        let ledger = Ledger::open_memory().unwrap();
        assert_eq!(ledger.balance("alice").unwrap(), 0);
        assert_eq!(ledger.fund("alice", 100).unwrap(), 100);
        assert_eq!(ledger.fund("alice", 50).unwrap(), 150);
        assert_eq!(ledger.balance("alice").unwrap(), 150);
    }

    #[test]
    fn settle_debits_consumer_credits_provider() {
        let ledger = Ledger::open_memory_with_price(1).unwrap();
        ledger.fund("consumer", 10).unwrap();

        let out = ledger
            .settle("job-1", "consumer", "provider", 1500)
            .unwrap();
        assert!(out.is_first());
        assert_eq!(out.credits(), 2); // ceil(1500/1000)=2

        assert_eq!(ledger.balance("consumer").unwrap(), 8);
        assert_eq!(ledger.balance("provider").unwrap(), 2);
    }

    #[test]
    fn settle_insufficient_credits() {
        let ledger = Ledger::open_memory_with_price(1).unwrap();
        ledger.fund("consumer", 1).unwrap();
        let err = ledger
            .settle("job-x", "consumer", "provider", 2500)
            .unwrap_err();
        assert!(matches!(
            err,
            LedgerError::InsufficientCredits {
                need: 3,
                have: 1,
                ..
            }
        ));
        assert_eq!(ledger.balance("consumer").unwrap(), 1);
        assert_eq!(ledger.balance("provider").unwrap(), 0);
        assert!(ledger.get_settlement("job-x").unwrap().is_none());
    }

    #[test]
    fn settle_idempotent_by_job_id() {
        let ledger = Ledger::open_memory_with_price(1).unwrap();
        ledger.fund("c", 100).unwrap();

        let first = ledger.settle("same-job", "c", "p", 1000).unwrap();
        assert!(matches!(first, SettleOutcome::Settled { credits: 1, .. }));

        let second = ledger.settle("same-job", "c", "p", 1000).unwrap();
        assert!(matches!(
            second,
            SettleOutcome::AlreadySettled { credits: 1, .. }
        ));

        // Replay with different args still returns original settlement; no double move.
        let third = ledger.settle("same-job", "c", "other", 9999).unwrap();
        assert_eq!(third.credits(), 1);
        assert!(!third.is_first());

        assert_eq!(ledger.balance("c").unwrap(), 99);
        assert_eq!(ledger.balance("p").unwrap(), 1);
        assert_eq!(ledger.balance("other").unwrap(), 0);
    }

    #[test]
    fn zero_token_job_records_without_transfer() {
        let ledger = Ledger::open_memory().unwrap();
        ledger.fund("c", 5).unwrap();
        let out = ledger.settle("empty", "c", "p", 0).unwrap();
        assert_eq!(out.credits(), 0);
        assert_eq!(ledger.balance("c").unwrap(), 5);
        assert_eq!(ledger.balance("p").unwrap(), 0);
        assert!(ledger.get_settlement("empty").unwrap().is_some());
    }

    #[test]
    fn sqlite_file_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");

        {
            let ledger = Ledger::open_sqlite(&path).unwrap();
            ledger.fund("c", 20).unwrap();
            ledger.settle("j1", "c", "p", 1000).unwrap();
        }

        let ledger = Ledger::open_sqlite(&path).unwrap();
        assert_eq!(ledger.balance("c").unwrap(), 19);
        assert_eq!(ledger.balance("p").unwrap(), 1);
        assert!(ledger.get_settlement("j1").unwrap().is_some());
    }

    #[test]
    fn concurrent_distinct_jobs_are_safe() {
        let ledger = Ledger::open_memory_with_price(1).unwrap();
        ledger.fund("consumer", 10_000).unwrap();

        let threads = 32usize;
        let mut handles = Vec::with_capacity(threads);
        for i in 0..threads {
            let ledger = ledger.clone();
            handles.push(thread::spawn(move || {
                let job = format!("job-{i}");
                ledger
                    .settle(&job, "consumer", "provider", 1000)
                    .expect("settle");
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(ledger.balance("consumer").unwrap(), 10_000 - threads as u64);
        assert_eq!(ledger.balance("provider").unwrap(), threads as u64);
    }

    #[test]
    fn concurrent_same_job_settles_once() {
        let ledger = Ledger::open_memory_with_price(1).unwrap();
        ledger.fund("consumer", 50).unwrap();

        let first_count = Arc::new(AtomicUsize::new(0));
        let already_count = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();

        for _ in 0..16 {
            let ledger = ledger.clone();
            let first_count = Arc::clone(&first_count);
            let already_count = Arc::clone(&already_count);
            handles.push(thread::spawn(move || {
                let out = ledger
                    .settle("dup-job", "consumer", "provider", 2000)
                    .expect("settle");
                match out {
                    SettleOutcome::Settled { credits: 2, .. } => {
                        first_count.fetch_add(1, Ordering::SeqCst);
                    }
                    SettleOutcome::AlreadySettled { credits: 2, .. } => {
                        already_count.fetch_add(1, Ordering::SeqCst);
                    }
                    other => panic!("unexpected outcome: {other:?}"),
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(first_count.load(Ordering::SeqCst), 1);
        assert_eq!(already_count.load(Ordering::SeqCst), 15);
        assert_eq!(ledger.balance("consumer").unwrap(), 48);
        assert_eq!(ledger.balance("provider").unwrap(), 2);
    }

    #[test]
    fn rejects_self_settle() {
        let ledger = Ledger::open_memory().unwrap();
        let err = ledger.settle("j", "node", "node", 10).unwrap_err();
        assert!(matches!(err, LedgerError::InvalidSettlement(_)));
    }
}
