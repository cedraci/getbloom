mod common;

use common::uniq;

#[tokio::test]
#[ignore = "requires postgres"]
async fn observation_currency_exists_and_is_append_only() {
    let pool = common::pool().await;
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq("CCY")).fetch_one(&pool).await.unwrap();
    let iid: i64 = sqlx::query_scalar(
        "INSERT INTO instrument DEFAULT VALUES RETURNING instrument_id")
        .fetch_one(&pool).await.unwrap();
    let fid: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,$2,'Last','numeric') RETURNING id")
        .bind(class).bind(uniq("CPX")).fetch_one(&pool).await.unwrap();
    let vid: i64 = sqlx::query_scalar(
        "INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("ccyv")).fetch_one(&pool).await.unwrap();
    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ok') RETURNING id")
        .bind(vid).fetch_one(&pool).await.unwrap();
    let basis: i16 = sqlx::query_scalar(
        "SELECT id FROM adjustment_basis WHERE adj_normal = false")
        .fetch_one(&pool).await.unwrap();
    let oid: i64 = sqlx::query_scalar(
        "INSERT INTO observation (instrument_id, field_id, obs_date, layer,
                                  basis_id, value_num, run_id, currency)
         VALUES ($1,$2,'2026-08-13','raw',$3,101.5,$4,'GBp') RETURNING id")
        .bind(iid).bind(fid).bind(basis).bind(rid)
        .fetch_one(&pool).await.unwrap();
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT currency FROM observation WHERE id = $1")
        .bind(oid).fetch_one(&pool).await.unwrap();
    assert_eq!(stored.as_deref(), Some("GBp"), "verbatim, pence stay pence");
    let tampered = sqlx::query(
        "UPDATE observation SET currency = 'GBP' WHERE id = $1")
        .bind(oid).execute(&pool).await;
    assert!(tampered.is_err(),
            "currency is as immutable as the value it prices");
}
