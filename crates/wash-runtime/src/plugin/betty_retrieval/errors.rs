//! Statement, pool and decode failures, as `betty-blocks:retrieval/types.error`.

use deadpool_postgres::{PoolError, TimeoutType};

use super::bindings::betty_blocks::retrieval::types::Error;
use super::bindings::wasmcloud::postgres::types::{DbError, Error as PgError};

/// A statement's failure. A database error keeps its SQLSTATE so a component
/// can branch on `code`; a parameter that could not encode as its column's
/// type is `invalid-params`; anything else is the connection's.
pub(crate) fn postgres(e: &tokio_postgres::Error) -> Error {
    if let Some(db) = e.as_db_error() {
        return Error::Postgres(PgError::QueryFailed(DbError {
            code: db.code().code().to_string(),
            severity: db.severity().to_string(),
            message: db.message().to_string(),
            detail: db.detail().map(ToString::to_string),
            extras: Vec::new(),
        }));
    }
    let message = with_sources(e);
    if caused_by_wrong_type(e) {
        Error::Postgres(PgError::InvalidParams(message))
    } else {
        Error::Postgres(PgError::ConnectionFailed(message))
    }
}

/// A failed checkout. Only a wait that outlasted the pool's timeout means the
/// pool is exhausted; a connection that could not be created or a closed pool
/// is a connection failure.
pub(crate) fn pool(e: &PoolError) -> Error {
    match e {
        PoolError::Timeout(TimeoutType::Wait) => Error::PoolExhausted,
        other => connection_failed(format!(
            "check out a pooled connection: {}",
            with_sources(other)
        )),
    }
}

pub(crate) fn connection_failed(message: String) -> Error {
    Error::Postgres(PgError::ConnectionFailed(message))
}

/// A returned value with no `pg-value` to become.
pub(crate) fn value_conversion(message: String) -> Error {
    Error::Postgres(PgError::ValueConversionFailed(message))
}

/// What `commit` reports for a transaction an earlier statement aborted.
/// Postgres answers that `COMMIT` with a `ROLLBACK` tag and no error, so
/// without this a component would believe its writes landed.
pub(crate) fn aborted_transaction(first_error: String) -> Error {
    Error::Postgres(PgError::QueryFailed(DbError {
        code: "25P02".to_string(),
        severity: "ERROR".to_string(),
        message:
            "the transaction was aborted by an earlier failed statement; nothing was committed"
                .to_string(),
        detail: Some(first_error),
        extras: Vec::new(),
    }))
}

fn caused_by_wrong_type(e: &(dyn std::error::Error + 'static)) -> bool {
    std::iter::successors(Some(e), |e| e.source()).any(|e| e.is::<postgres_types::WrongType>())
}

/// `e` and each of its sources: tokio-postgres's own `Display` names only the
/// kind of failure ("error serializing parameter 0"), never its cause, while
/// deadpool's already ends with its source, which is not repeated.
fn with_sources(e: &(dyn std::error::Error + 'static)) -> String {
    let mut message = String::new();
    for err in std::iter::successors(Some(e), |e| e.source()) {
        let text = err.to_string();
        let text = text.trim();
        if message.ends_with(text) {
            continue;
        }
        if !message.is_empty() {
            message.push_str(": ");
        }
        message.push_str(text);
    }
    message
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct SerializeFailed(postgres_types::WrongType);

    impl std::fmt::Display for SerializeFailed {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("error serializing parameter 0")
        }
    }

    impl std::error::Error for SerializeFailed {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    /// Like deadpool's `PoolError`: its `Display` already ends with its source.
    #[derive(Debug)]
    struct CreateFailed(SerializeFailed);

    impl std::fmt::Display for CreateFailed {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "error occurred while creating a new object: {}", self.0)
        }
    }

    impl std::error::Error for CreateFailed {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    fn query_failed(err: Error) -> Option<DbError> {
        match err {
            Error::Postgres(PgError::QueryFailed(db)) => Some(db),
            _ => None,
        }
    }

    #[test]
    fn an_aborted_transaction_is_sqlstate_25p02_with_the_first_error_as_detail() {
        let first = "duplicate key value violates unique constraint \"entity_pkey\"";
        let db = query_failed(aborted_transaction(first.to_string()))
            .expect("an aborted transaction is reported as query-failed");
        assert_eq!(db.code, "25P02");
        assert_eq!(db.severity, "ERROR");
        assert_eq!(
            db.message,
            "the transaction was aborted by an earlier failed statement; nothing was committed"
        );
        assert_eq!(db.detail.as_deref(), Some(first));
        assert!(db.extras.is_empty());
    }

    #[test]
    fn only_a_wait_timeout_is_pool_exhausted() {
        assert!(matches!(
            pool(&PoolError::Timeout(TimeoutType::Wait)),
            Error::PoolExhausted
        ));
        assert!(matches!(
            pool(&PoolError::Timeout(TimeoutType::Create)),
            Error::Postgres(PgError::ConnectionFailed(_))
        ));
        assert!(matches!(
            pool(&PoolError::Closed),
            Error::Postgres(PgError::ConnectionFailed(_))
        ));
    }

    #[test]
    fn a_wrong_type_anywhere_in_the_source_chain_is_detected() {
        let wrong = postgres_types::WrongType::new::<i32>(postgres_types::Type::TEXT);
        assert!(caused_by_wrong_type(&SerializeFailed(wrong)));
        assert!(!caused_by_wrong_type(&std::io::Error::other(
            "connection reset by peer"
        )));
    }

    #[test]
    fn a_message_carries_every_source() {
        let wrong = postgres_types::WrongType::new::<i32>(postgres_types::Type::TEXT);
        assert_eq!(
            with_sources(&SerializeFailed(wrong)),
            "error serializing parameter 0: cannot convert between the Rust type `i32` and the \
             Postgres type `text`"
        );
    }

    #[test]
    fn a_source_its_wrapper_already_prints_is_not_repeated() {
        let wrong = postgres_types::WrongType::new::<i32>(postgres_types::Type::TEXT);
        assert_eq!(
            with_sources(&CreateFailed(SerializeFailed(wrong))),
            "error occurred while creating a new object: error serializing parameter 0: cannot \
             convert between the Rust type `i32` and the Postgres type `text`"
        );
    }

    #[test]
    fn a_decode_failure_is_value_conversion_failed() {
        assert!(matches!(
            value_conversion("column 0: unsupported type".to_string()),
            Error::Postgres(PgError::ValueConversionFailed(m)) if m == "column 0: unsupported type"
        ));
    }
}
