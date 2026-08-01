//! P2P inference credits ledger.
//!
//! Transfer credits from consumers to providers on completed jobs, priced per
//! 1 000 tokens (stub). Settlement is **idempotent** on `job_id` and safe under
//! concurrent retries.
//!
//! Backends: in-memory SQLite (`Ledger::open_memory`) or file SQLite
//! (`Ledger::open_sqlite`). See `docs/p2p/ECONOMICS.md` for the MVP faucet model.

mod error;
mod ledger;
mod pricing;

pub use error::LedgerError;
pub use ledger::{Ledger, SettleOutcome, SettlementRecord};
pub use pricing::{credits_for_tokens, DEFAULT_PRICE_CREDITS_PER_1K};
