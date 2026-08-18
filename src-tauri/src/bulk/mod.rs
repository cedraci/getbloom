//! Bulk asset management through an Excel round trip.
//!
//! Three files with hard boundaries, because that boundary is what makes the
//! interesting logic testable without Postgres or Excel:
//!   sheet.rs  -- files only, never the database
//!   diff.rs   -- pure functions, neither files nor the database
//!   mod.rs    -- the only place that does both

pub mod diff;
pub mod sheet;
