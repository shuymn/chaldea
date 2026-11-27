//! The Chaldea daemon: the event store (`docs/design/event-store.md`), the HTTP server
//! (`docs/design/http-surface.md`), and the process lifecycle (`docs/design/daemon-lifecycle.md`).
//!
//! The store's rules (§2.1 of that design):
//!
//! 1. **SQLite is the proof, Rust is the second line.** Every invariant the schema can
//!    express is a `CHECK`, `UNIQUE`, foreign key, or trigger that ends the transaction with
//!    `RAISE(ROLLBACK)`; Rust checks the same conditions first only to return typed errors.
//! 2. **No cross-call state outside the database.** Heads, cursors, bindings, receipts,
//!    deliveries, and projections are read inside the transaction that depends on them; only
//!    the immutable epoch is cached.
//! 3. **One transaction per job, and nothing commits after an error.** A job is a closure
//!    over [`store::WriteTx`]; the first error poisons it and the writer rolls back.
//! 4. **Projections change only through one function**, used by live appends and by rebuild
//!    on the same events in the same order.

// The shared test support (`tests/support`) names this crate by its package name, so the
// unit tests can include the same module.
#[cfg(test)]
extern crate self as chaldea_daemon;

mod attach;
mod deadline;
pub mod hook;
pub mod http;
pub mod lifecycle;
pub mod store;
pub mod supervisor;
mod transcript;
