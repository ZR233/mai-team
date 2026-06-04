use crate::{Result, StoreError};
use std::time::Duration;

const SQLITE_BUSY_RETRY_SECS: u64 = 30;
const SQLITE_BUSY_RETRY_DELAY_MS: u64 = 250;

pub(crate) async fn retry_sqlite_busy<F, Fut, T>(mut operation: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(SQLITE_BUSY_RETRY_SECS);
    loop {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(err) if sqlite_busy_error(&err) && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(SQLITE_BUSY_RETRY_DELAY_MS)).await;
            }
            Err(err) => return Err(err),
        }
    }
}

pub(crate) fn sqlite_busy_error(err: &StoreError) -> bool {
    match err {
        StoreError::Sqlite(err) => matches!(
            err.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        ),
        StoreError::Toasty(err) => {
            err.to_string().contains("database is locked")
                || err.to_string().contains("database is busy")
        }
        StoreError::Io(_)
        | StoreError::Json(_)
        | StoreError::Toml(_)
        | StoreError::TomlSer(_)
        | StoreError::Time(_)
        | StoreError::Parse(_)
        | StoreError::InvalidConfig(_) => false,
    }
}
