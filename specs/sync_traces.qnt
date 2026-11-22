// -*- mode: Bluespec; -*-
/**
 * Named deterministic traces of `sync` (docs/design/formal-specs.md §7). Each `trace_*`
 * run is exported as ITF to `specs/traces/sync/` and its `decide_replay` calls are
 * replayed against the real function by crates/core/tests/quint_sync_traces.rs.
 *
 * - `trace_refused_*` runs end where the client discards a response the contract says
 *   must not change its state (an older request generation) or does not switch epoch.
 * - `trace_coverage_*` runs reach `decide_replay` branches the protocol never takes.
 * - `trace_finding_*` runs reach a gap in the contract with steps the model allows; each
 *   ends with an `expect` that states it (docs/design/formal-specs.md §7.4).
 *
 * Client 1 follows both streams; client 2 follows the set given to `initWith`. Request
 * ids count from 1 in issue order.
 */
module sync_traces {
  import sync.* from "./sync"

  pure def cp(s: State, c: int, x: int): Copy = s.clients.get(c).copies.get(x)
  pure def seqs(k: Copy): List[int] = k.events.foldl([], (l, e) => l.append(e.seq))
  pure def due(s: State, c: int, x: int): bool = s.clients.get(c).due.get(x)
  pure def epochOf(s: State, c: int): int = s.clients.get(c).epoch

  /// mvp §2.4 "Replay": forward pages from the cursor with `has_more`, then a
  /// `stream_advanced` makes the next page due.
  run trace_forward_paging =
    initWith(2, 0, false, Set(HOST))
      .then(connectA(1))
      .then(serverSubscribeA(1))
      .then(recvNoteA(1))
      .then(fetchA(1, HOST, 1))
      .then(serveA(1))
      .expect(planned(last.calls, PForward({ after: 0, limit: 1 })))
      .then(deliverA(1))
      .expect(seqs(cp(st, 1, HOST)) == [1] and due(st, 1, HOST))
      .then(fetchA(1, HOST, 1))
      .then(serveA(2))
      .then(deliverA(2))
      .expect(seqs(cp(st, 1, HOST)) == [1, 2] and not(due(st, 1, HOST)))
      .then(appendA(HOST, SurfaceUpdate, 1))
      .then(recvNoteA(1))
      .expect(due(st, 1, HOST))
      .then(fetchA(1, HOST, 2))
      .then(serveA(3))
      .then(deliverA(3))
      .expect(seqs(cp(st, 1, HOST)) == [1, 2, 3] and cp(st, 1, HOST).proj == { surface: 3, status: 0 })

  /// mvp §2.4 "Snapshot fallback" and "Backward history": too many events from the start
  /// give a snapshot with its recent events, and backward pages fill in the older ones.
  run trace_snapshot_then_backward =
    initWith(3, 0, false, Set(HOST))
      .then(fetchA(1, HOST, 2))
      .then(serveA(1))
      .expect(planned(last.calls, PSnapshot(TooManyEvents)))
      .then(deliverA(1))
      .expect(seqs(cp(st, 1, HOST)) == [3] and cp(st, 1, HOST).hi == 3 and cp(st, 1, HOST).proj == { surface: 2, status: 1 })
      .then(fetchBackA(1, HOST, 1))
      .then(serveA(2))
      .expect(planned(last.calls, PBackward({ before: 3, limit: 1 })))
      .then(deliverA(2))
      .expect(seqs(cp(st, 1, HOST)) == [2, 3])
      .then(fetchBackA(1, HOST, 2))
      .then(serveA(3))
      .then(deliverA(3))
      .expect(seqs(cp(st, 1, HOST)) == [1, 2, 3])

  /// mvp §2.4: the byte bound gives a snapshot; a replay exactly at both bounds (events and
  /// bytes) is a forward page.
  run trace_byte_bound =
    initWith(2, 0, true, Set(HOST))
      .then(fetchA(1, HOST, 2))
      .then(serveA(1))
      .expect(planned(last.calls, PSnapshot(TooManyBytes)))
      .then(deliverA(1))
      .expect(seqs(cp(st, 1, HOST)) == [2])
      .then(appendA(HOST, Plain, 1))
      .then(appendA(HOST, Plain, 3))
      .then(fetchA(1, HOST, 2))
      .then(serveA(2))
      .expect(planned(last.calls, PForward({ after: 2, limit: 2 })))
      .then(deliverA(2))
      .expect(seqs(cp(st, 1, HOST)) == [2, 3, 4])

  /// mvp §2.4: after a new database, the reconnect's refetch sends the old epoch, the
  /// forward response is an `epoch_changed` snapshot, and the client switches epoch and
  /// every local copy together, then refetches the other stream.
  run trace_epoch_change =
    initWith(1, 1, false, Set(THREAD))
      .then(fetchA(1, HOST, 2))
      .then(serveA(1))
      .then(deliverA(1))
      .then(fetchA(1, THREAD, 2))
      .then(serveA(2))
      .then(deliverA(2))
      .expect(epochOf(st, 1) == 1 and cp(st, 1, HOST).hi == 1 and cp(st, 1, THREAD).hi == 1)
      .then(reset)
      .then(appendA(HOST, Plain, 1))
      .then(appendA(HOST, Plain, 1))
      .then(connectA(1))
      .then(fetchA(1, HOST, 2))
      .then(serveA(3))
      .expect(planned(last.calls, PSnapshot(EpochChanged)))
      .then(deliverA(3))
      .expect(epochOf(st, 1) == 2 and cp(st, 1, HOST).hi == 2 and cp(st, 1, THREAD) == EMPTY_COPY and due(st, 1, THREAD))
      .then(fetchA(1, THREAD, 2))
      .then(serveA(4))
      .then(deliverA(4))
      .expect(cp(st, 1, THREAD).epoch == 2 and cp(st, 1, THREAD).hi == 0)
      .then(quiesce)
      .expect(converged(st))

  /// mvp §2.4 "a response from an older request generation is discarded": a page the
  /// client stopped waiting for arrives after the newer page; applying it would duplicate
  /// events 1 and 2.
  run trace_refused_older_generation =
    initWith(2, 0, false, Set(HOST))
      .then(fetchA(1, HOST, 2))
      .then(serveA(1))
      .then(timeoutA(1, HOST, Fwd))
      .then(fetchA(1, HOST, 2))
      .then(serveA(2))
      .then(deliverA(2))
      .expect(seqs(cp(st, 1, HOST)) == [1, 2])
      .then(deliverA(1))
      .expect(seqs(cp(st, 1, HOST)) == [1, 2] and st.clients.get(1) == last.prevClients.get(1))

  /// The generation covers every stream: a page of the old epoch for the thread stream,
  /// requested before the switch on the host stream, arrives after it. Were it current, as
  /// a forward response with another epoch it would switch the client back to epoch 1.
  run trace_refused_stale_epoch_after_switch =
    initWith(1, 1, false, Set(THREAD))
      .then(fetchA(1, HOST, 2))
      .then(serveA(1))
      .then(deliverA(1))
      .then(fetchA(1, THREAD, 2))
      .then(serveA(2))
      .then(deliverA(2))
      .then(appendA(THREAD, Plain, 1))
      .then(fetchA(1, THREAD, 2))
      .then(serveA(3))
      .then(reset)
      .then(fetchA(1, HOST, 2))
      .then(serveA(4))
      .then(deliverA(4))
      .expect(epochOf(st, 1) == 2)
      .then(deliverA(3))
      .expect(epochOf(st, 1) == 2 and cp(st, 1, THREAD) == EMPTY_COPY)

  /// mvp §2.4 "Backward history": a backward page of another epoch does not switch the
  /// client; it makes a forward resync due, and that forward response switches.
  run trace_refused_backward_epoch_switch =
    initWith(3, 0, false, Set(HOST))
      .then(fetchA(1, HOST, 2))
      .then(serveA(1))
      .then(deliverA(1))
      .then(reset)
      .then(appendA(HOST, Plain, 1))
      .then(appendA(HOST, Plain, 1))
      .then(appendA(HOST, Plain, 1))
      .then(fetchBackA(1, HOST, 1))
      .then(serveA(2))
      .expect(planned(last.calls, PBackward({ before: 3, limit: 1 })))
      .then(deliverA(2))
      .expect(seqs(cp(st, 1, HOST)) == [3] and epochOf(st, 1) == 1 and due(st, 1, HOST))
      .then(fetchA(1, HOST, 2))
      .then(serveA(3))
      .expect(planned(last.calls, PSnapshot(EpochChanged)))
      .then(deliverA(3))
      .expect(epochOf(st, 1) == 2 and cp(st, 1, HOST).hi == 3 and cp(st, 1, HOST).events[0].epoch == 2)

  /// A snapshot that replaces a copy makes the backward request built from the old copy
  /// of an older generation; prepending its page (event 2) to the new copy (event 5)
  /// would leave a gap.
  run trace_refused_backward_after_snapshot =
    initWith(3, 0, false, Set(HOST))
      .then(fetchA(1, HOST, 2))
      .then(serveA(1))
      .then(deliverA(1))
      .then(fetchBackA(1, HOST, 1))
      .then(serveA(2))
      .then(appendA(HOST, Plain, 3))
      .then(appendA(HOST, Plain, 3))
      .then(fetchA(1, HOST, 2))
      .then(serveA(3))
      .expect(planned(last.calls, PSnapshot(TooManyBytes)))
      .then(deliverA(3))
      .expect(seqs(cp(st, 1, HOST)) == [5])
      .then(deliverA(2))
      .expect(seqs(cp(st, 1, HOST)) == [5])

  /// mvp §2.4 "A heads refresh becomes due every 30 s": a lost `stream_advanced` leaves the
  /// thread stream behind with nothing due; the refresh recovers it.
  run trace_lost_notification_recovered =
    initWith(1, 0, false, Set(HOST))
      .then(connectA(1))
      .then(serverSubscribeA(1))
      .then(recvNoteA(1))
      .then(fetchA(1, HOST, 2))
      .then(serveA(1))
      .then(deliverA(1))
      .then(fetchA(1, THREAD, 2))
      .then(serveA(2))
      .then(deliverA(2))
      .then(appendA(THREAD, StatusChange, 1))
      .then(dropNote(1, 0))
      .expect(not(due(st, 1, THREAD)) and cp(st, 1, THREAD).hi == 0)
      .then(quiesce)
      .expect(last.act == Quiesce({ onlyHeads: true }) and converged(st))

  /// mvp §2.4 "the client refetches on every reconnect": a commit while the socket was
  /// down produced no notification.
  run trace_reconnect_after_restart =
    initWith(0, 0, false, Set(HOST))
      .then(connectA(1))
      .then(serverSubscribeA(1))
      .then(recvNoteA(1))
      .then(fetchA(1, HOST, 2))
      .then(serveA(1))
      .then(deliverA(1))
      .then(restart)
      .then(appendA(HOST, Plain, 1))
      .then(connectA(1))
      .expect(due(st, 1, HOST))
      .then(fetchA(1, HOST, 2))
      .then(serveA(2))
      .then(deliverA(2))
      .expect(cp(st, 1, HOST).hi == 1)

  /// http-surface §8 D15: subscribes in quick succession are answered by one `heads` frame
  /// for the latest set.
  run trace_coalesced_subscribe =
    initWith(1, 1, false, Set(HOST))
      .then(connectA(2))
      .then(resubscribeA(2, Set(THREAD)))
      .then(resubscribeA(2, STREAMS))
      .then(serverSubscribeA(2))
      .expect(st.sockets.get(2).queue == [Heads(Map(HOST -> 1, THREAD -> 1))] and not(st.sockets.get(2).hasPending))

  /// `decide_replay` branches the protocol does not take: `limit` out of range (checked
  /// first), `after_seq` with `before_seq`, the default limit, `CursorAhead`, and
  /// `after_seq` at the head with the largest limit.
  run trace_coverage_queries =
    initWith(2, 0, false, Set(HOST))
      .then(rawFetch(1, HOST, { after: NONE, before: NONE, limit: 0, epoch: 0 }))
      .then(serveA(1))
      .expect(planned(last.calls, PRefused(LimitOutOfRange)))
      .then(deliverA(1))
      .then(rawFetch(1, HOST, { after: NONE, before: NONE, limit: LIMIT_MAX + 1, epoch: 0 }))
      .then(serveA(2))
      .expect(planned(last.calls, PRefused(LimitOutOfRange)))
      .then(deliverA(2))
      .then(rawFetch(1, HOST, { after: 1, before: 1, limit: 0, epoch: 0 }))
      .then(serveA(3))
      .expect(planned(last.calls, PRefused(LimitOutOfRange)))
      .then(deliverA(3))
      .then(rawFetch(1, HOST, { after: 0, before: 1, limit: NONE, epoch: 0 }))
      .then(serveA(4))
      .expect(planned(last.calls, PRefused(AfterAndBefore)))
      .then(deliverA(4))
      .then(rawFetch(1, HOST, { after: NONE, before: NONE, limit: NONE, epoch: 0 }))
      .then(serveA(5))
      .expect(planned(last.calls, PForward({ after: 0, limit: LIMIT_DEFAULT })))
      .then(deliverA(5))
      .expect(seqs(cp(st, 1, HOST)) == [1, 2])
      .then(rawFetch(1, HOST, { after: 5, before: NONE, limit: 1, epoch: 1 }))
      .then(serveA(6))
      .expect(planned(last.calls, PSnapshot(CursorAhead)))
      .then(deliverA(6))
      .then(rawFetch(1, HOST, { after: 2, before: NONE, limit: LIMIT_MAX, epoch: 1 }))
      .then(serveA(7))
      .expect(planned(last.calls, PForward({ after: 2, limit: LIMIT_MAX })))
      .then(deliverA(7))
      .expect(seqs(cp(st, 1, HOST)) == [2])

  /// A host snapshot carries each thread's `ThreadInfo` (axes and attention) as of its
  /// read, but those change through thread-stream events; the host stream does not advance,
  /// so a client following only the host stream converges on events and surfaces and still
  /// shows the old status.
  run trace_finding_cross_stream_snapshot_field =
    initWith(3, 1, false, Set(HOST))
      .then(connectA(2))
      .then(serverSubscribeA(2))
      .then(recvNoteA(2))
      .then(fetchA(2, HOST, 2))
      .then(serveA(1))
      .then(deliverA(1))
      .expect(cp(st, 2, HOST).threadStatus == 0)
      .then(appendA(THREAD, StatusChange, 1))
      .then(quiesce)
      .expect(converged(st) and cp(st, 2, HOST).threadStatus == 0 and projOf(logOf(st.host, 1, THREAD)).status == 1)
}
