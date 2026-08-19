use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("bloomberg fetch failed (exit {code}): {detail}")]
    Blp { code: i32, detail: String },
    #[error("sidecar error: {0}")]
    Sidecar(String),
    #[error("validation error: {0}")]
    Validation(String),
    #[error("cannot delete: {reason} ({})", format_counts(.counts))]
    DeleteBlocked { reason: String, counts: Vec<(String, i64)> },
    #[error("import rejected: {reason}")]
    ImportRejected { reason: String },
}

/// Render the blocking counts as "3 assets, 2 fields" for the Display impl.
/// The structured `counts` stay available to callers; this is only the text.
fn format_counts(counts: &[(String, i64)]) -> String {
    if counts.is_empty() {
        return "no details".into();
    }
    counts.iter()
        .map(|(what, n)| format!("{n} {what}"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub type AppResult<T> = Result<T, AppError>;

// Tauri commands need serializable errors.
impl serde::Serialize for AppError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}
