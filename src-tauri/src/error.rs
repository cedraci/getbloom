use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("excel error: {0}")]
    Excel(String),
    #[error("refresh driver failed (exit {code}): {detail}")]
    Refresh { code: i32, detail: String },
    #[error("validation error: {0}")]
    Validation(String),
}

pub type AppResult<T> = Result<T, AppError>;

// Tauri commands need serializable errors.
impl serde::Serialize for AppError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}
