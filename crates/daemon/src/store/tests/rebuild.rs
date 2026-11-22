//! Drop-and-rebuild equality (PR6-CL-7).

use proptest::prelude::*;

use super::helpers::RawEvent;
use super::support::{Model, TestStore, jobs, run_ops, steps};
use crate::store::open::{expected_dump, supported_version};
use crate::store::project::{compare_projections, dump_projections, projection_tables};
use crate::store::{StoreError, WriteTx};

/// Inserts an event of an unknown kind on the thread's stream with raw SQL.
fn raw_unknown_kind(
    tx: &mut WriteTx<'_>,
    thread: chaldea_core::ids::ThreadId,
    key: &str,
) -> Result<(), StoreError> {
    RawEvent::new(
        &format!("thread:{thread}"),
        key,
        "future_kind",
        r#"{"from":"a newer daemon"}"#,
    )
    .insert(tx.raw())
    .map(|_| ())
    .map_err(StoreError::from)
}

/// Drops every projection table and recreates it from the shipped DDL.
fn drop_and_recreate(tx: &WriteTx<'_>) {
    let c = tx.raw();
    let tables = projection_tables(c).unwrap();
    // Children first: proj_threads is every other table's parent.
    for t in tables
        .iter()
        .filter(|t| *t != "proj_threads")
        .chain(std::iter::once(&"proj_threads".to_owned()))
    {
        c.execute_batch(&format!("DROP TABLE \"{t}\"")).unwrap();
    }
    for row in expected_dump(supported_version()).unwrap() {
        if row[0] == "table" && row[1].starts_with("proj_") {
            c.execute_batch(&row[3]).unwrap();
        }
    }
    assert_eq!(projection_tables(c).unwrap(), tables);
}

proptest! {
    /// After any history (every kind, annexes, advances without events, generation changes
    /// across paths, empty surfaces, launchless inputs, task pairs, unknown kinds inserted
    /// raw, blob content, failed jobs), dropping the projection tables and rebuilding them
    /// from `events` (with another clock) reproduces the maintained rows, except the
    /// permitted transcript rewind.
    #[test]
    fn rebuild_equals_maintained(steps in steps()) {
        let s = TestStore::new();
        let mut model = Model::default();
        for (job, _) in jobs(&mut model, &steps, true, false) {
            let commits = job.commits;
            let ops = job.ops.clone();
            let r = s.run(move |tx| run_ops(tx, ops, &raw_unknown_kind));
            prop_assert_eq!(r.is_ok(), commits, "job {:?}: {:?}", job.ops, r.err());
        }
        let maintained = s.run(|tx| Ok(dump_projections(tx.raw())?)).unwrap();
        s.clock.set("2031-01-01T00:00:00.000Z");
        let rebuilt = s
            .run(|tx| {
                drop_and_recreate(tx);
                tx.rebuild_projections()?;
                Ok(dump_projections(tx.raw())?)
            })
            .unwrap();
        let diff = compare_projections(&maintained, &rebuilt);
        prop_assert!(diff.is_empty(), "{:#?}", diff);
    }
}

/// The projection dump keys rows by every primary-key column: two surfaces of one stream
/// are two rows.
#[test]
fn projection_dump_keeps_two_surfaces_in_one_stream() {
    use chaldea_core::event::TypedEvent;
    use chaldea_core::ids::{StreamId, SurfaceId};
    use chaldea_core::payload::{Component, SurfaceScope, SurfaceUpdate};

    let s = TestStore::new();
    s.append(
        ["a", "b"]
            .into_iter()
            .map(|id| {
                super::support::event(
                    StreamId::Host,
                    id,
                    TypedEvent::SurfaceUpdate(SurfaceUpdate {
                        surface_id: SurfaceId(id.into()),
                        scope: SurfaceScope::Host,
                        components: vec![Component::Text { text: id.into() }],
                    }),
                )
            })
            .collect(),
    )
    .unwrap();

    let counts = s
        .run(|tx| {
            let sql_count: i64 = tx
                .raw()
                .query_row("SELECT count(*) FROM proj_surfaces", [], |row| row.get(0))
                .map_err(StoreError::from)?;
            let dump = dump_projections(tx.raw())?;
            Ok((sql_count, dump["proj_surfaces"].rows.len()))
        })
        .unwrap();

    assert_eq!(counts, (2, 2));
}
