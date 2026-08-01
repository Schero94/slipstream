use thiserror::Error;

/// Ledger operation errors.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LedgerError {
    #[error("insufficient credits: account={account} need={need} have={have}")]
    InsufficientCredits {
        account: String,
        need: u64,
        have: u64,
    },

    #[error("invalid settlement: {0}")]
    InvalidSettlement(String),

    #[error("storage error: {0}")]
    Storage(String),
}

impl From<rusqlite::Error> for LedgerError {
    fn from(value: rusqlite::Error) -> Self {
        LedgerError::Storage(value.to_string())
    }
}
