use crate::book::BookEntry;
use crate::error::AppResult;
use crate::fields::FieldDef;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct View {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub active: bool,
}

pub async fn create_view(pool: &PgPool, name: &str, description: &str) -> AppResult<View> {
    Ok(sqlx::query_as::<_, View>(
        "INSERT INTO view (name, description) VALUES ($1,$2) RETURNING *",
    )
    .bind(name)
    .bind(description)
    .fetch_one(pool)
    .await?)
}

pub async fn list_views(pool: &PgPool) -> AppResult<Vec<View>> {
    Ok(sqlx::query_as::<_, View>("SELECT * FROM view ORDER BY name")
        .fetch_all(pool)
        .await?)
}

pub async fn set_view_instruments(
    pool: &PgPool,
    view_id: i64,
    instrument_ids: &[i64],
) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM view_instrument WHERE view_id = $1")
        .bind(view_id)
        .execute(&mut *tx)
        .await?;
    for iid in instrument_ids {
        sqlx::query("INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)")
            .bind(view_id)
            .bind(iid)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn set_view_fields(
    pool: &PgPool,
    view_id: i64,
    field_ids: &[i64],
) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM view_field WHERE view_id = $1")
        .bind(view_id)
        .execute(&mut *tx)
        .await?;
    for fid in field_ids {
        sqlx::query("INSERT INTO view_field (view_id, field_id) VALUES ($1,$2)")
            .bind(view_id)
            .bind(fid)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The active, resolved members of a view.
///
/// An instrument with a pending `resolution_review` is excluded (spec §5). The
/// alternative -- fetching for an identifier nobody has confirmed -- produces a
/// time series that looks complete and is attached to the wrong company.
///
/// Under the current design this exclusion is vacuous: `book::BookEntry` has no
/// `review_pending` flag because an ambiguous resolution writes no book entry
/// at all (see 0a19e48) -- that absence is the real enforcement. The
/// `NOT EXISTS` guard below stays anyway, written correctly against
/// `resolution_review` joined to `resolution_decision.chosen_instrument_id`, in
/// case a later phase opens a review against an instrument that is already
/// bound and already has a book entry.
pub async fn view_instruments(pool: &PgPool, view_id: i64) -> AppResult<Vec<BookEntry>> {
    // A single query scoped to this view, not `book::list` (one
    // `current_security` query per row in the WHOLE book) filtered down in
    // Rust afterward -- `estimate_view` calls this per view and the views
    // screen calls `estimate_view` per view on every load, so the old
    // approach cost views x book_size queries for a screen render.
    let today = chrono::Local::now().date_naive();
    Ok(sqlx::query_as::<_, BookEntry>(
        "SELECT b.instrument_id, b.asset_class_id, b.label, b.active, b.note,
                sec.value AS security
           FROM view_instrument vi
           JOIN book_entry b ON b.instrument_id = vi.instrument_id
           LEFT JOIN LATERAL (
             SELECT value FROM instrument_alias
              WHERE instrument_id = b.instrument_id AND id_type = 'bdp_security'
                AND valid_from <= $2 AND valid_to > $2
                AND system_to = 'infinity'
              ORDER BY valid_from DESC LIMIT 1
           ) sec ON true
          WHERE vi.view_id = $1 AND b.active
            AND NOT EXISTS (
              SELECT 1 FROM resolution_review r
                JOIN resolution_decision d ON d.id = r.decision_id
               WHERE r.status = 'pending' AND d.chosen_instrument_id = vi.instrument_id)
          ORDER BY b.label")
        .bind(view_id).bind(today)
        .fetch_all(pool).await?)
}

/// A field as the planner sees it: its definition plus the one attribute that
/// is not on the row -- the **effective** cadence,
/// `COALESCE(field_def.cadence, asset_class.default_cadence)` (P11 11.1, the
/// same COALESCE idiom `quality.rs` uses for `qc_stale_days`).
///
/// `fetch_via` needs no resolution: it lives on `field_def` and rides along
/// inside `def`. Both are flattened on the wire, so the shape the UI already
/// consumes gains a key and loses none.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ViewField {
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub def: FieldDef,
    pub effective_cadence: String,
}

pub async fn view_fields(pool: &PgPool, view_id: i64) -> AppResult<Vec<ViewField>> {
    let explicit = sqlx::query_as::<_, ViewField>(
        "SELECT f.*, COALESCE(f.cadence, ac.default_cadence) AS effective_cadence
           FROM field_def f
           JOIN asset_class ac ON ac.id = f.asset_class_id
           JOIN view_field vf ON vf.field_id = f.id
         WHERE vf.view_id = $1 AND f.active ORDER BY f.asset_class_id, f.mnemonic",
    )
    .bind(view_id)
    .fetch_all(pool)
    .await?;
    if !explicit.is_empty() {
        return Ok(explicit);
    }
    // Spec default: all active fields of the classes present in the view's instruments.
    Ok(sqlx::query_as::<_, ViewField>(
        "SELECT DISTINCT f.*, COALESCE(f.cadence, ac.default_cadence) AS effective_cadence
           FROM field_def f
           JOIN asset_class ac ON ac.id = f.asset_class_id
           JOIN book_entry b ON b.asset_class_id = f.asset_class_id
           JOIN view_instrument vi ON vi.instrument_id = b.instrument_id
         WHERE vi.view_id = $1 AND f.active AND b.active
         ORDER BY f.asset_class_id, f.mnemonic",
    )
    .bind(view_id)
    .fetch_all(pool)
    .await?)
}
