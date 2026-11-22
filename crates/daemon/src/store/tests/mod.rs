//! Unit tests that need privileged access: raw SQL on production-configured connections,
//! fault points, panics outside a job, dropped projection tables, and a second store built
//! with another projection version (`docs/design/event-store.md` §7). Tests that need only
//! the public API are the integration tests in `crates/daemon/tests/`.

#[path = "../../../tests/support/mod.rs"]
pub(crate) mod support;

mod annex;
mod helpers;
mod open;
mod readers;
mod rebuild;
mod schema_tests;
mod triggers;
mod writer;
