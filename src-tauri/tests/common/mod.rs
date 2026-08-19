#![allow(dead_code)]
//! Shared harness for integration tests. Each test gets a pool against
//! bloom_test with migrations applied.
//!
//! These tests share ONE database, run on parallel threads, and never clean up
//! after themselves -- the same arrangement `db_integration.rs` already uses.
//! `uniq()` is what keeps them from colliding with each other and with previous
//! runs; use it for every value that sits under a UNIQUE constraint.

use sqlx::PgPool;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static SEQ: AtomicU32 = AtomicU32::new(0);

pub async fn pool() -> PgPool {
    let url = std::env::var("BLOOM_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost/bloom_test".into());
    let pool = PgPool::connect(&url).await.expect("connect to bloom_test");
    sqlx::migrate!("./migrations").run(&pool).await.expect("migrations");
    pool
}

/// Unique fixture value, distinct both ACROSS tests in one run and ACROSS
/// repeated runs. Copied from the convention in `db_integration.rs`.
pub fn uniq(stem: &str) -> String {
    static TAG: OnceLock<String> = OnceLock::new();
    let tag = TAG.get_or_init(|| {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        format!("{:x}", nanos % 0x100000)
    });
    format!("{stem}{tag}{}", SEQ.fetch_add(1, Ordering::Relaxed))
}
