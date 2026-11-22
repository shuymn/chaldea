// -*- mode: Bluespec; -*-
/**
 * Named deterministic traces of `sync` (docs/design/formal-specs.md §7.5). Each `trace_*`
 * run is exported as ITF to `specs/traces/sync/` and its `decide_replay` calls are
 * replayed against the real function by crates/core/tests/quint_sync_traces.rs.
 *
 * - `trace_refused_*` runs end where the client discards a response the contract says
 *   must not change its state (an older request generation) or does not switch epoch.
 * - `trace_coverage_*` runs reach `decide_replay` branches the protocol never takes.
 *
 * Client 1 follows both streams; client 2 follows the set given to `initWith`. Request
 * ids count from 1 in issue order.
 */
module sync_traces {
  import sync.* from "./sync"

  pure def cp(s: State, c: int, x: int): Copy = s.clients.get(c).copies.get(x)
  pure def seqs(k: Copy): List[int] = k.events.foldl([], (l, e) => l.append(e.seq))
  pure def due(s: State, c: int, x: int): bool = dueOf(s.clients.get(c), x)
  pure def epochOf(s: State, c: int): int = s.clients.get(c).epoch
  pure def page(s: State, id: int): { events: List[Ev], headSeq: int, hasMore: bool, hasMoreBefore: bool } =
    match s.resps.get(id).body {
      | Page(pg) => pg
      | _ => { events: [], headSeq: -1, hasMore: false, hasMoreBefore: false }
    }

  // Client 1 connected, its subscribe answered and the `heads` frame taken in.
  run connected1 =
    initWith(1, 0, false, Set(HOST))
      .then(connectA(1))
      .then(serverSubscribeA(1))
      .then(sendFrameA(1))
      .then(recvNoteA(1))

  /// mvp §2.4 "Replay": forward pages from the cursor with `has_more` and
  /// `has_more_before`, then a `stream_advanced` makes the next page due.
  run trace_forward_paging =
    initWith(2, 0, false, Set(HOST))
      .then(connectA(1))
      .then(fetchA(1, HOST, 1))
      .then(serveA(1))
      .expect(planned(last.calls, PForward({ after: 0, limit: 1 })))
      .expect(page(st, 1).hasMore and not(page(st, 1).hasMoreBefore))
      .then(deliverA(1))
      // No head seen yet: only `has_more` makes the next page due.
      .expect(seqs(cp(st, 1, HOST)) == [1] and due(st, 1, HOST) and not(cp(st, 1, HOST).moreBefore))
      .then(fetchA(1, HOST, 1))
      .then(serveA(2))
      .expect(not(page(st, 2).hasMore) and page(st, 2).hasMoreBefore)
      .then(deliverA(2))
      .expect(seqs(cp(st, 1, HOST)) == [1, 2] and not(due(st, 1, HOST)))
      .then(serverSubscribeA(1))
      .then(sendFrameA(1))
      .then(recvNoteA(1))
      .expect(not(due(st, 1, HOST)))
      .then(appendA(HOST, SurfaceUpdate, 1))
      .then(sendFrameA(1))
      .then(recvNoteA(1))
      .expect(due(st, 1, HOST))
      .then(fetchA(1, HOST, 2))
      .then(serveA(3))
      .then(deliverA(3))
      .expect(seqs(cp(st, 1, HOST)) == [1, 2, 3] and cp(st, 1, HOST).proj == { surface: 3, status: 0 })

  /// mvp §2.4 "Snapshot fallback" and "Backward history" for the host stream: a host
  /// snapshot carries no events (`HostSnapshot`), so the first backward page is before
  /// `as_of_seq + 1`; `has_more_before` ends the paging.
  run trace_host_snapshot_then_backward =
    initWith(3, 0, false, Set(HOST))
      .then(fetchA(1, HOST, 2))
      .then(serveA(1))
      .expect(planned(last.calls, PSnapshot(TooManyEvents)))
      .then(deliverA(1))
      .expect(seqs(cp(st, 1, HOST)) == [] and cp(st, 1, HOST).hi == 3 and cp(st, 1, HOST).moreBefore)
      .expect(cp(st, 1, HOST).proj == { surface: 2, status: 1 })
      .then(fetchBackA(1, HOST, 1))
      .then(serveA(2))
      .expect(planned(last.calls, PBackward({ before: 4, limit: 1 })) and page(st, 2).hasMoreBefore)
      .then(deliverA(2))
      .expect(seqs(cp(st, 1, HOST)) == [3] and cp(st, 1, HOST).moreBefore)
      .then(fetchBackA(1, HOST, 2))
      .then(serveA(3))
      .expect(not(page(st, 3).hasMoreBefore))
      .then(deliverA(3))
      .expect(seqs(cp(st, 1, HOST)) == [1, 2, 3] and not(cp(st, 1, HOST).moreBefore))

  /// The same for a thread stream, whose snapshot carries its recent events.
  run trace_thread_snapshot_then_backward =
    initWith(0, 3, false, Set(HOST))
      .then(fetchA(1, THREAD, 2))
      .then(serveA(1))
      .then(deliverA(1))
      .expect(seqs(cp(st, 1, THREAD)) == [3] and cp(st, 1, THREAD).moreBefore)
      .then(fetchBackA(1, THREAD, 2))
      .then(serveA(2))
      .expect(planned(last.calls, PBackward({ before: 3, limit: 2 })) and not(page(st, 2).hasMoreBefore))
      .then(deliverA(2))
      .expect(seqs(cp(st, 1, THREAD)) == [1, 2, 3] and not(cp(st, 1, THREAD).moreBefore))

  /// mvp §2.4: the byte bound gives a snapshot; a replay exactly at both bounds (events and
  /// bytes) is a forward page.
  run trace_byte_bound =
    initWith(2, 0, true, Set(HOST))
      .then(fetchA(1, HOST, 2))
      .then(serveA(1))
      .expect(planned(last.calls, PSnapshot(TooManyBytes)))
      .then(deliverA(1))
      .expect(seqs(cp(st, 1, HOST)) == [] and cp(st, 1, HOST).hi == 2)
      .then(appendA(HOST, Plain, 1))
      .then(appendA(HOST, Plain, 3))
      .then(fetchA(1, HOST, 2))
      .then(serveA(2))
      .expect(planned(last.calls, PForward({ after: 2, limit: 2 })))
      .then(deliverA(2))
      .expect(seqs(cp(st, 1, HOST)) == [3, 4])

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
      .then(reset(true))
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

  /// A new database without the thread: its id is unknown there, so a fetch is 404 and
  /// the client stops following it; the socket's set and `heads` leave it out.
  run trace_vanished_stream =
    initWith(1, 1, false, Set(THREAD))
      .then(fetchA(1, HOST, 2))
      .then(serveA(1))
      .then(deliverA(1))
      .then(fetchA(1, THREAD, 2))
      .then(serveA(2))
      .then(deliverA(2))
      .then(reset(false))
      .then(connectA(1))
      .then(serverSubscribeA(1))
      .expect(st.sockets.get(1).subs == Set(HOST))
      .then(fetchA(1, THREAD, 2))
      .then(serveA(3))
      .expect(st.resps.get(3).body == NotFound)
      .then(deliverA(3))
      .expect(st.clients.get(1).want == Set(HOST) and cp(st, 1, THREAD) == EMPTY_COPY and st.sockets.get(1).pending == Set(HOST))
      .then(quiesce)
      .expect(converged(st))

  /// A read that fails (408 at its cutoff, 503 `busy`) leaves the fetch due again.
  run trace_read_failed_retried =
    initWith(1, 0, false, Set(HOST))
      .then(fetchA(1, HOST, 2))
      .then(failReadA(1))
      .then(deliverA(1))
      .expect(due(st, 1, HOST) and cp(st, 1, HOST) == EMPTY_COPY)
      .then(fetchA(1, HOST, 2))
      .then(serveA(2))
      .then(deliverA(2))
      .expect(cp(st, 1, HOST).hi == 1)

  /// F2 (1), (4): a page the client gave up on arrives after the newer page; applying it
  /// would duplicate events 1 and 2.
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

  /// F2 (2): a page of the old epoch for the thread stream, requested before the switch on
  /// the host stream, arrives after it. Were it current, as a forward response with
  /// another epoch it would switch the client back to epoch 1.
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
      .then(reset(true))
      .then(fetchA(1, HOST, 2))
      .then(serveA(4))
      .then(deliverA(4))
      .expect(epochOf(st, 1) == 2)
      .then(deliverA(3))
      .expect(epochOf(st, 1) == 2 and cp(st, 1, THREAD) == EMPTY_COPY)

  /// F2 (2) at the first establishment: before the client holds an epoch, a host page of
  /// epoch 1 is in transit; the database is replaced; the thread page of epoch 2
  /// establishes the epoch. The host page, requested before that, is of an older
  /// generation; were it current, it would switch the client back to epoch 1.
  run trace_refused_stale_epoch_at_first_establishment =
    initWith(1, 1, false, Set(HOST))
      .then(fetchA(1, HOST, 2))
      .then(serveA(1))
      .then(reset(true))
      .then(fetchA(1, THREAD, 2))
      .then(serveA(2))
      .then(deliverA(2))
      .expect(epochOf(st, 1) == 2 and due(st, 1, HOST))
      .then(deliverA(1))
      .expect(epochOf(st, 1) == 2 and cp(st, 1, HOST) == EMPTY_COPY)

  /// mvp §2.4 "Backward history": a backward page with another epoch does not switch the
  /// client; it makes a forward resync due, and that forward response switches.
  run trace_refused_backward_epoch_switch =
    initWith(3, 0, false, Set(HOST))
      .then(fetchA(1, HOST, 2))
      .then(serveA(1))
      .then(deliverA(1))
      .then(reset(true))
      .then(appendA(HOST, Plain, 1))
      .then(appendA(HOST, Plain, 1))
      .then(appendA(HOST, Plain, 1))
      .then(fetchBackA(1, HOST, 1))
      .then(serveA(2))
      .expect(planned(last.calls, PBackward({ before: 4, limit: 1 })))
      .then(deliverA(2))
      .expect(seqs(cp(st, 1, HOST)) == [] and epochOf(st, 1) == 1 and due(st, 1, HOST))
      .then(fetchA(1, HOST, 2))
      .then(serveA(3))
      .expect(planned(last.calls, PSnapshot(EpochChanged)))
      .then(deliverA(3))
      .expect(epochOf(st, 1) == 2 and cp(st, 1, HOST).hi == 3 and cp(st, 1, HOST).epoch == 2)

  /// F2 (3): a snapshot that replaces a copy makes the backward request built from the old
  /// copy of an older generation; prepending its page (event 2) to the new copy (event 5)
  /// would leave a gap.
  run trace_refused_backward_after_snapshot =
    initWith(0, 3, false, Set(HOST))
      .then(fetchA(1, THREAD, 2))
      .then(serveA(1))
      .then(deliverA(1))
      .then(fetchBackA(1, THREAD, 1))
      .then(serveA(2))
      .then(appendA(THREAD, Plain, 3))
      .then(appendA(THREAD, Plain, 3))
      .then(fetchA(1, THREAD, 2))
      .then(serveA(3))
      .expect(planned(last.calls, PSnapshot(TooManyBytes)))
      .then(deliverA(3))
      .expect(seqs(cp(st, 1, THREAD)) == [5])
      .then(deliverA(2))
      .expect(seqs(cp(st, 1, THREAD)) == [5])

  /// mvp §2.4 "Backward history": a backward page of another epoch asks for a forward
  /// resync. A forward snapshot of the old epoch, requested before that, is still current
  /// and is applied; the resync it did not answer stays due, and the next forward fetch
  /// switches the client to the new epoch.
  run trace_resync_kept_after_stale_snapshot =
    initWith(3, 0, false, Set(HOST))
      .then(fetchA(1, HOST, 2))
      .then(serveA(1))
      .then(deliverA(1))
      .expect(cp(st, 1, HOST).hi == 3 and cp(st, 1, HOST).moreBefore)
      .then(appendA(HOST, Plain, 3))
      .then(appendA(HOST, Plain, 3))
      .then(fetchA(1, HOST, 2))
      .then(serveA(2))
      .expect(planned(last.calls, PSnapshot(TooManyBytes)))
      .then(reset(true))
      .then(fetchBackA(1, HOST, 1))
      .then(serveA(3))
      .then(deliverA(3))
      // The resync waits for the forward request in flight.
      .expect(epochOf(st, 1) == 1 and st.clients.get(1).need.get(HOST) and not(due(st, 1, HOST)))
      .then(deliverA(2))
      .expect(epochOf(st, 1) == 1 and cp(st, 1, HOST).hi == 5 and due(st, 1, HOST))
      .then(fetchA(1, HOST, 2))
      .then(serveA(4))
      .expect(planned(last.calls, PSnapshot(EpochChanged)))
      .then(deliverA(4))
      .expect(epochOf(st, 1) == 2)

  /// A `stream_advanced` for a commit after the page was read arrives while the fetch is in
  /// flight; the page, without `has_more`, leaves the stream due (the head observed is
  /// newer than the cursor).
  run trace_notified_during_fetch =
    connected1
      .then(fetchA(1, HOST, 2))
      .then(serveA(1))
      .then(appendA(HOST, Plain, 1))
      .then(sendFrameA(1))
      .then(recvNoteA(1))
      .then(deliverA(1))
      .expect(cp(st, 1, HOST).hi == 1 and due(st, 1, HOST))
      .then(fetchA(1, HOST, 2))
      .then(serveA(2))
      .then(deliverA(2))
      .expect(cp(st, 1, HOST).hi == 2 and not(due(st, 1, HOST)))

  /// The other order: the page arrives first, then the notification.
  run trace_notified_after_response =
    connected1
      .then(fetchA(1, HOST, 2))
      .then(serveA(1))
      .then(appendA(HOST, Plain, 1))
      .then(deliverA(1))
      .expect(not(due(st, 1, HOST)))
      .then(sendFrameA(1))
      .then(recvNoteA(1))
      .expect(due(st, 1, HOST))

  /// mvp §2.4 "A heads refresh becomes due every 30 s": a lost `stream_advanced` leaves the
  /// stream behind with nothing due; the refresh recovers it.
  run trace_lost_notification_recovered =
    connected1
      .then(fetchA(1, HOST, 2))
      .then(serveA(1))
      .then(deliverA(1))
      .then(appendA(HOST, StatusChange, 1))
      .then(sendFrameA(1))
      .then(dropNote(1))
      .expect(not(due(st, 1, HOST)) and cp(st, 1, HOST).hi == 1)
      .then(quiesce)
      .expect(last.act == Quiesce({ onlyHeads: true }) and converged(st))

  /// Refreshes due before the frame is sent coalesce into one `heads` frame (http §2.6).
  run trace_heads_coalesce =
    connected1
      .then(headsRefreshA(1))
      .then(headsRefreshA(1))
      .then(headsRefreshA(1))
      .then(appendA(HOST, Plain, 1))
      .then(headsRefreshA(1))
      .then(sendFrameA(1))
      .expect(st.sockets.get(1).queue == [Heads(Map(HOST -> 2, THREAD -> 0))] and not(canSend(st.sockets.get(1))))

  /// mvp §2.4 "the client refetches on every reconnect": a commit while the socket was
  /// down produced no notification.
  run trace_reconnect_after_restart =
    initWith(0, 0, false, Set(HOST))
      .then(connectA(1))
      .then(serverSubscribeA(1))
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

  /// http §8 D15: subscribes in quick succession are answered by one `heads` frame for the
  /// latest set; an empty set and the same set are subscribes too.
  run trace_coalesced_subscribe =
    initWith(1, 1, false, Set(HOST))
      .then(connectA(2))
      .then(resubscribeA(2, Set(THREAD)))
      .then(resubscribeA(2, STREAMS))
      .then(serverSubscribeA(2))
      .then(sendFrameA(2))
      .expect(st.sockets.get(2).queue == [Heads(Map(HOST -> 1, THREAD -> 1))] and not(st.sockets.get(2).hasPending))
      .then(recvNoteA(2))
      .then(resubscribeA(2, Set()))
      .then(serverSubscribeA(2))
      .then(sendFrameA(2))
      .expect(st.sockets.get(2).queue == [Heads(Map())] and st.clients.get(2).want == Set())
      .then(recvNoteA(2))
      .then(resubscribeA(2, Set(HOST)))
      .then(resubscribeA(2, Set(HOST)))
      .then(serverSubscribeA(2))
      .then(sendFrameA(2))
      .expect(st.sockets.get(2).queue == [Heads(Map(HOST -> 1))])

  /// `decide_replay` branches the protocol does not take: `limit` out of range (checked
  /// first), `after_seq` with `before_seq`, the default limit, `CursorAhead`, `after_seq`
  /// at the head with the largest limit, and the order of the snapshot reasons when two
  /// hold at once (an epoch change before a cursor ahead, too many events before too many
  /// bytes).
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
      .then(rawFetch(1, HOST, { after: 9, before: NONE, limit: 1, epoch: 2 }))
      .then(serveA(8))
      .expect(planned(last.calls, PSnapshot(EpochChanged)))
      .then(loseResp(8))
      .then(timeoutA(1, HOST, Fwd))
      .then(appendA(HOST, Plain, 3))
      .then(appendA(HOST, Plain, 3))
      .then(appendA(HOST, Plain, 3))
      .then(rawFetch(1, HOST, { after: 2, before: NONE, limit: 1, epoch: 1 }))
      .then(serveA(9))
      .expect(planned(last.calls, PSnapshot(TooManyEvents)))

  /// F1 (formal-specs §7.4): a host snapshot carries each thread's `ThreadInfo` as of the
  /// same database read, but axes and attention change through thread-stream events, so
  /// the host stream does not advance. The field is an initial value: a client renders it
  /// from the thread stream, which converges, while the host copy keeps the value read.
  run trace_cross_stream_field_from_owner =
    initWith(3, 1, false, Set(HOST))
      .then(connectA(1))
      .then(serverSubscribeA(1))
      .then(fetchA(1, HOST, 2))
      .then(serveA(1))
      .then(deliverA(1))
      .expect(cp(st, 1, HOST).threadStatus == 0)
      .then(appendA(THREAD, StatusChange, 1))
      .then(quiesce)
      .expect(and {
        converged(st),
        cp(st, 1, HOST).threadStatus == 0,
        cp(st, 1, THREAD).proj.status == 1,
        shownThreadStatus(st.clients.get(1)) == 1,
        projOf(logOf(st.host, 1, THREAD)).status == 1,
      })
}
