use diesel::r2d2;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DatabaseError {
    #[error("Connection error: {0}")]
    Connection(String),

    #[error("Query error: {0}")]
    Query(String),

    #[error("Migration error: {0}")]
    Migration(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Duplicate entry: {0}")]
    Duplicate(String),

    #[error("Invalid data: {0}")]
    InvalidData(String),

    #[error("Pool error: {0}")]
    Pool(String),
}

impl From<diesel::result::Error> for DatabaseError {
    fn from(err: diesel::result::Error) -> Self {
        match err {
            diesel::result::Error::NotFound => {
                DatabaseError::NotFound("record not found".to_string())
            }
            diesel::result::Error::DatabaseError(kind, info) => {
                let msg = info.message().to_string();
                match kind {
                    diesel::result::DatabaseErrorKind::UniqueViolation => {
                        DatabaseError::Duplicate(msg)
                    }
                    diesel::result::DatabaseErrorKind::ForeignKeyViolation => {
                        DatabaseError::InvalidData(msg)
                    }
                    _ => DatabaseError::Query(msg),
                }
            }
            other => DatabaseError::Query(other.to_string()),
        }
    }
}

impl From<r2d2::Error> for DatabaseError {
    fn from(err: r2d2::Error) -> Self {
        DatabaseError::Pool(err.to_string())
    }
}

pub type DatabaseResult<T> = Result<T, DatabaseError>;
