// ferrox-cp: control plane for the Ferrox LLM gateway
// Phase 3 — data layer implemented; HTTP endpoints in a later milestone.
#![allow(dead_code)]
mod config;
mod db;
mod state;

/// Migrations bundled into the binary at compile time.
/// Also re-used by integration tests via `#[sqlx::test(migrator = &MIGRATOR)]`.
// sqlx::migrate! requires a path with a parent component.
// "./migrations" resolves relative to CARGO_MANIFEST_DIR (crate root).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

fn main() {
    println!("ferrox-cp: control plane (Phase 3 in progress)");
}
