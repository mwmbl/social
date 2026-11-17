//! Error type for the store.

use thiserror::Error;

/// Everything that can go wrong in the store. Each variant says what failed and on which
/// URI, host or registration.
#[derive(Debug, Error)]
pub enum StoreError {
    /// A SQLite call failed. `operation` names the call and its subject.
    #[error("{operation}: {source}")]
    Db {
        operation: String,
        #[source]
        source: rusqlite::Error,
    },
    /// A column held a string that is not a known value of its enum.
    #[error("unknown {kind} value {value:?}")]
    UnknownValue { kind: &'static str, value: String },
    /// An update targeted a row that does not exist.
    #[error("{operation}: no such row ({subject})")]
    NotFound {
        operation: &'static str,
        subject: String,
    },
    /// The database was written by a newer version of this program.
    #[error("database schema version {found} is newer than supported version {supported}")]
    SchemaTooNew { found: usize, supported: usize },
    /// A migration left rows whose foreign keys point nowhere; it was rolled back.
    #[error("{operation}: {violations} foreign key violation(s)")]
    ForeignKeys { operation: String, violations: i64 },
    /// A value was outside the range the database can hold.
    #[error("{operation}: value out of range ({detail})")]
    OutOfRange {
        operation: &'static str,
        detail: String,
    },
}

/// Builds a mapper that attaches `operation` to a rusqlite error. An unknown enum string read
/// back from a column is surfaced as [`StoreError::UnknownValue`] rather than a generic error.
pub(crate) fn ctx(operation: impl Into<String>) -> impl FnOnce(rusqlite::Error) -> StoreError {
    move |e| match e {
        rusqlite::Error::FromSqlConversionFailure(idx, ty, inner) => {
            match inner.downcast::<StoreError>() {
                Ok(store_error) => *store_error,
                Err(inner) => StoreError::Db {
                    operation: operation.into(),
                    source: rusqlite::Error::FromSqlConversionFailure(idx, ty, inner),
                },
            }
        }
        source => StoreError::Db {
            operation: operation.into(),
            source,
        },
    }
}

/// Turns "zero rows updated" into [`StoreError::NotFound`].
pub(crate) fn expect_changed(
    changed: usize,
    operation: &'static str,
    subject: impl FnOnce() -> String,
) -> Result<(), StoreError> {
    if changed == 0 {
        Err(StoreError::NotFound {
            operation,
            subject: subject(),
        })
    } else {
        Ok(())
    }
}

pub(crate) fn to_usize(n: i64, operation: &'static str) -> Result<usize, StoreError> {
    usize::try_from(n).map_err(|_| StoreError::OutOfRange {
        operation,
        detail: format!("{n} is not a usize"),
    })
}
