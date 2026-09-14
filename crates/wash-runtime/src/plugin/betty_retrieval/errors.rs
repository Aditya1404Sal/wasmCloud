//! Statement, pool and decode failures, as `betty-blocks:retrieval/types.error`.

use deadpool_postgres::{PoolError, TimeoutType};

use super::bindings::betty_blocks::retrieval::types::Error;
use super::bindings::wasmcloud::postgres::types::{DbError, Error as PgError};

/// A bind parameter tokio-postgres could not encode. It displays as the
/// original error, which is also its source.
#[derive(Debug)]
pub(crate) struct ParamEncodeError(Box<dyn std::error::Error + Sync + Send>);

impl ParamEncodeError {
    pub(crate) fn new(source: Box<dyn std::error::Error + Sync + Send>) -> Self {
        Self(source)
    }
}

impl std::fmt::Display for ParamEncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for ParamEncodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.0)
    }
}

/// A statement's failure. A database error keeps its SQLSTATE so a component
/// can branch on `code`; anything else failed before reaching the database.
pub(crate) fn postgres(e: &tokio_postgres::Error) -> Error {
    match e.as_db_error() {
        Some(db) => Error::Postgres(PgError::QueryFailed(DbError {
            code: db.code().code().to_string(),
            severity: db.severity().to_string(),
            message: db.message().to_string(),
            detail: db.detail().map(ToString::to_string),
            extras: Vec::new(),
        })),
        None => client_side(e),
    }
}

/// A failure the database did not report. A bind parameter's fault is
/// `invalid-params`, which no retry fixes; anything else is the connection's.
fn client_side(e: &(dyn std::error::Error + 'static)) -> Error {
    let message = with_sources(e);
    let parameter = std::iter::successors(Some(e), |e| e.source())
        .any(|e| e.is::<ParamEncodeError>() || e.is::<postgres_types::WrongType>());
    if parameter {
        invalid_params(message)
    } else {
        connection_failed(message)
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

pub(crate) fn invalid_params(message: String) -> Error {
    Error::Postgres(PgError::InvalidParams(message))
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

/// `e` and each of its sources: tokio-postgres's own `Display` names only the
/// kind of failure ("error serializing parameter 0"), never its cause, while
/// deadpool's already ends with its source, which is not repeated.
pub(crate) fn with_sources(e: &(dyn std::error::Error + 'static)) -> String {
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

    /// Like tokio-postgres's `Error`: `Display` names only the kind of failure,
    /// and the cause is left to `source`.
    #[derive(Debug)]
    struct KindOnly(&'static str, Box<dyn std::error::Error + Sync + Send>);

    impl std::fmt::Display for KindOnly {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for KindOnly {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&*self.1)
        }
    }

    /// Like deadpool's `PoolError`: its `Display` already ends with its source.
    #[derive(Debug)]
    struct CreateFailed(KindOnly);

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

    fn serializing(cause: impl std::error::Error + Sync + Send + 'static) -> KindOnly {
        KindOnly("error serializing parameter 0", Box::new(cause))
    }

    fn wrong_type() -> postgres_types::WrongType {
        postgres_types::WrongType::new::<i32>(postgres_types::Type::TEXT)
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
    fn a_parameter_that_failed_to_encode_is_invalid_params() {
        let encode = ParamEncodeError::new("invalid character in a UUID".into());
        assert!(matches!(
            client_side(&serializing(encode)),
            Error::Postgres(PgError::InvalidParams(m))
                if m == "error serializing parameter 0: invalid character in a UUID"
        ));
    }

    #[test]
    fn a_value_of_the_wrong_rust_type_is_invalid_params() {
        assert!(matches!(
            client_side(&serializing(wrong_type())),
            Error::Postgres(PgError::InvalidParams(_))
        ));
    }

    #[test]
    fn a_failure_no_parameter_caused_is_connection_failed() {
        assert!(matches!(
            client_side(&std::io::Error::other("connection reset by peer")),
            Error::Postgres(PgError::ConnectionFailed(m)) if m == "connection reset by peer"
        ));
    }

    #[test]
    fn a_message_carries_every_nested_cause() {
        let decode = KindOnly(
            "error deserializing column 2",
            Box::new(KindOnly(
                "unsupported type [vector]",
                "no pg-value decodes it".into(),
            )),
        );
        assert_eq!(
            with_sources(&decode),
            "error deserializing column 2: unsupported type [vector]: no pg-value decodes it"
        );
    }

    #[test]
    fn a_source_its_wrapper_already_prints_is_not_repeated() {
        assert_eq!(
            with_sources(&CreateFailed(serializing(wrong_type()))),
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
