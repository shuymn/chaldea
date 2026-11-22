// -*- mode: Bluespec; -*-
/**
 * Q-B, read side: the Sync Seam protocol between one authoritative host and PWA-like
 * clients (docs/mvp-design.md §2.4 "Replay", "Backward history", "Live notifications";
 * docs/design/formal-specs.md §7).
 *
 * Two layers, as in Q-A:
 *
 * - `decideReplay` mirrors `crates/core/src/sync.rs` `decide_replay` branch for branch,
 *   with its event and byte bounds scaled down (one model seq stands for
 *   10 000 / REPLAY_MAX_EVENTS real ones, one size unit for 8 MiB / REPLAY_MAX_BYTES
 *   bytes; every comparison in `decide_replay` is linear, so scaling keeps each branch).
 *   Every step records its calls (`last.calls`), and the committed traces replay against
 *   the real function (crates/core/tests/quint_sync_traces.rs).
 * - The protocol layer is the contract: the host answers each request from one read
 *   snapshot (PR 6's store guarantees are assumed: atomic commit, gapless per-stream
 *   `seq`, the epoch fixed at database creation), the network loses, delays, and reorders
 *   responses and loses notifications, and the client keeps one epoch, a local copy per
 *   stream, and request generations.
 *
 * Abstracted: the host is one database with an epoch; a new database (a new epoch) starts
 * empty; events carry their identity (epoch, stream, seq) as a ghost; the rendered state
 * of a stream is a projection of its events (`Proj`: the latest surface and a status
 * count); time is explicit actions (a heads refresh, a client's request timeout).
 * Not modelled: authorization and 404s, blobs, tickets, the socket's read and send
 * deadlines (a socket that misses them is a `disconnect`), the WebSocket `hello`.
 */
module sync {
  // ---------------------------------------------------------------------------
  // Bounds
  // ---------------------------------------------------------------------------

  pure val HOST = 1
  pure val THREAD = 2
  pure val STREAMS = Set(HOST, THREAD)
  pure val CLIENTS = Set(1, 2)
  pure val MAX_EPOCHS = 2
  // Events per stream and epoch.
  pure val MAX_EVENTS = 5
  // Requests plus responses in the network at once.
  pure val MAX_IN_FLIGHT = 4
  // The bounded-liveness check: deterministic steps after connectivity returns (fewer
  // than 40 were needed over the CI simulation).
  pure val DRAIN_STEPS = 60
  pure val DRAIN_REFRESHES = 2
  // An omitted query parameter.
  pure val NONE = -1

  pure def min2(a: int, b: int): int = if (a < b) a else b
  pure def max2(a: int, b: int): int = if (a > b) a else b
  pure def minOf(s: Set[int]): int = s.fold(1000000, (a, b) => min2(a, b))
  pure def without(m: a -> b, k: a): a -> b = m.keys().exclude(Set(k)).mapBy(x => m.get(x))

  // ---------------------------------------------------------------------------
  // Reducer layer: crates/core/src/sync.rs `decide_replay`
  // ---------------------------------------------------------------------------

  pure val LIMIT_MIN = 1
  pure val LIMIT_MAX = 1000
  pure val LIMIT_DEFAULT = 200
  // Scaled bounds (see the module comment); the replay test scales back.
  pure val REPLAY_MAX_EVENTS = 2
  pure val REPLAY_MAX_BYTES = 4
  // Events carried in a snapshot (`SNAPSHOT_RECENT_EVENTS`, 50 in the code; the store
  // computes snapshots, so this is not replayed).
  pure val SNAPSHOT_RECENT_EVENTS = 1

  // `EventsQuery`: NONE for an omitted `after_seq`, `before_seq`, or `limit`; epoch 0 for
  // an omitted `epoch`.
  type Query = { after: int, before: int, limit: int, epoch: int }
  type StreamHead = { headSeq: int, cumHead: int, cumAfter: int }
  type Reason = EpochChanged | CursorAhead | TooManyEvents | TooManyBytes
  type QueryError = LimitOutOfRange | AfterAndBefore
  type Plan =
    | PForward({ after: int, limit: int })
    | PBackward({ before: int, limit: int })
    | PSnapshot(Reason)
    | PRefused(QueryError)

  /// `sync::decide_replay`.
  pure def decideReplay(q: Query, h: StreamHead, epoch: int): Plan =
    val limit = if (q.limit == NONE) LIMIT_DEFAULT else q.limit
    val after = if (q.after == NONE) 0 else q.after
    if (limit < LIMIT_MIN or limit > LIMIT_MAX) PRefused(LimitOutOfRange)
    else if (q.before != NONE)
      if (q.after != NONE) PRefused(AfterAndBefore) else PBackward({ before: q.before, limit: limit })
    else if (q.epoch != 0 and q.epoch != epoch) PSnapshot(EpochChanged)
    else if (after > h.headSeq) PSnapshot(CursorAhead)
    else if (h.headSeq - after > REPLAY_MAX_EVENTS) PSnapshot(TooManyEvents)
    else if (max2(h.cumHead - h.cumAfter, 0) > REPLAY_MAX_BYTES) PSnapshot(TooManyBytes)
    else PForward({ after: after, limit: limit })

  // ---------------------------------------------------------------------------
  // Host: one database, authoritative
  // ---------------------------------------------------------------------------

  type Kind = Plain | SurfaceUpdate | StatusChange
  // An event as stored. `epoch`, `stream`, `seq` are its identity (a ghost: a real event's
  // id is random per database); `size` stands for its payload bytes.
  type Ev = { epoch: int, stream: int, seq: int, size: int, kind: Kind }

  // What a client renders from a stream: the latest surface (its event's seq) and a status
  // counter (the thread's axes). A snapshot carries it as of `as_of_seq`.
  type Proj = { surface: int, status: int }
  pure val NO_PROJ: Proj = { surface: 0, status: 0 }

  pure def applyEv(p: Proj, e: Ev): Proj = match e.kind {
    | Plain => p
    | SurfaceUpdate => { ...p, surface: e.seq }
    | StatusChange => { ...p, status: p.status + 1 }
  }

  pure def projOf(evs: List[Ev]): Proj = evs.foldl(NO_PROJ, applyEv)
  pure def cumAt(evs: List[Ev], n: int): int = evs.slice(0, n).foldl(0, (a, e) => a + e.size)

  // `log` keeps every epoch's events: a client may still hold an older epoch's.
  type Host = { epoch: int, log: int -> (int -> List[Ev]) }

  pure def logOf(h: Host, e: int, s: int): List[Ev] = h.log.get(e).get(s)
  pure def headOf(h: Host, s: int): int = logOf(h, h.epoch, s).length()

  // ---------------------------------------------------------------------------
  // Network and client
  // ---------------------------------------------------------------------------

  type Dir = Fwd | Bwd
  type Req = { id: int, client: int, stream: int, dir: Dir, q: Query }
  type Body =
    | Page({ events: List[Ev], headSeq: int, hasMore: bool, hasMoreBefore: bool })
    // `threadStatus`: in a host snapshot, the thread's status read in the same snapshot
    // (`ThreadInfo`, a field of another stream); NONE in a thread snapshot.
    | Snap({ asOf: int, recent: List[Ev], proj: Proj, threadStatus: int, reason: Reason })
    | BadQuery(QueryError)
  type Resp = { req: Req, epoch: int, body: Body }

  type Note = Advanced({ stream: int, head: int }) | Heads(int -> int)

  // The server's side of one client's socket: the subscription set `S`, the latest
  // subscribe not yet answered (a later one replaces it), and the frames in order.
  type Socket = { open: bool, subs: Set[int], pending: Set[int], hasPending: bool, queue: List[Note] }
  pure val CLOSED: Socket = { open: false, subs: Set(), pending: Set(), hasPending: false, queue: [] }

  // A client's local copy of one stream. `epoch` 0: no local state. `hi` is the cursor:
  // everything up to it is known (from a snapshot or events); `events` are the envelopes
  // held, ending at `hi`. `threadStatus`: from the last host snapshot (host stream only).
  type Copy = { epoch: int, events: List[Ev], hi: int, proj: Proj, threadStatus: int }
  pure val EMPTY_COPY: Copy = { epoch: 0, events: [], hi: 0, proj: NO_PROJ, threadStatus: NONE }

  pure def loOf(cp: Copy): int = if (cp.events.length() == 0) cp.hi + 1 else cp.events[0].seq

  // `fwd` and `bwd` name the current request per stream and direction (0: none): the
  // latest one the client issued, until it is answered, times out, or the client replaces
  // that stream's copy, switches epoch, or reconnects. A response to any other request is
  // of an older generation. `lastFwd`, `lastBwd`, and `resetAt` record the same facts for
  // the invariants (ghosts).
  type Client = {
    epoch: int,
    copies: int -> Copy,
    fwd: int -> int,
    bwd: int -> int,
    due: int -> bool,
    want: Set[int],
    lastFwd: int -> int,
    lastBwd: int -> int,
    resetAt: int -> int,
  }

  pure def zeros(x: int): int = 0

  pure def newClient(want: Set[int]): Client = {
    epoch: 0,
    copies: STREAMS.mapBy(_ => EMPTY_COPY),
    fwd: STREAMS.mapBy(zeros),
    bwd: STREAMS.mapBy(zeros),
    due: STREAMS.mapBy(x => want.contains(x)),
    want: want,
    lastFwd: STREAMS.mapBy(zeros),
    lastBwd: STREAMS.mapBy(zeros),
    resetAt: STREAMS.mapBy(zeros),
  }

  type State = {
    host: Host,
    clients: int -> Client,
    sockets: int -> Socket,
    reqs: int -> Req,
    resps: int -> Resp,
    nextId: int,
  }

  type Call = DecideReplay({ q: Query, head: StreamHead, epoch: int, plan: Plan })

  type Action =
    | Init({ hostEvents: int, threadEvents: int, big: bool, want2: Set[int], replayMaxEvents: int, replayMaxBytes: int })
    | Append({ stream: int, kind: Kind, size: int })
    | Restart
    | ResetDb
    | Connect(int)
    | Disconnect(int)
    // `replacedPending`: the socket still held an unanswered subscribe.
    | Resubscribe({ client: int, want: Set[int], replacedPending: bool })
    | Fetch(Req)
    | FetchBack(Req)
    | RawFetch(Req)
    | Timeout({ client: int, stream: int, dir: Dir })
    | Serve(Req)
    | LoseReq(Req)
    | LoseResp(Req)
    | Deliver(Req)
    | ServerSubscribe(int)
    | HeadsRefresh(int)
    | RecvNote({ client: int, note: Note })
    | DropNote({ client: int, note: Note })
    // `onlyHeads`: once connectivity returned, some followed stream was behind the host
    // with no fetch due and no notification for it pending: only a heads refresh
    // recovers it.
    | Quiesce({ onlyHeads: bool })

  type Last = { act: Action, calls: List[Call], prevClients: int -> Client }

  var st: State
  var last: Last

  // ---------------------------------------------------------------------------
  // Transitions as pure functions (the actions and the drain share them)
  // ---------------------------------------------------------------------------

  type Out = { st: State, calls: List[Call] }

  pure def start(s: State): Out = { st: s, calls: [] }
  pure def andThen(o: Out, f: State => Out): Out = { val o2 = f(o.st) { st: o2.st, calls: o.calls.concat(o2.calls) } }

  pure def kindOf(seq: int): Kind = if (seq == 2) SurfaceUpdate else if (seq == 3) StatusChange else Plain

  pure def initialLog(e: int, hostEvents: int, threadEvents: int, sz: int): int -> List[Ev] =
    STREAMS.mapBy(x => {
      val n = if (x == HOST) hostEvents else threadEvents
      range(1, n + 1).foldl([], (l, k) => l.append({ epoch: e, stream: x, seq: k, size: sz, kind: kindOf(k) }))
    })

  pure def initialState(hostEvents: int, threadEvents: int, big: bool, want2: Set[int]): State = {
    host: { epoch: 1, log: Map(1 -> initialLog(1, hostEvents, threadEvents, if (big) 3 else 1)) },
    clients: Map(1 -> newClient(STREAMS), 2 -> newClient(want2)),
    sockets: CLIENTS.mapBy(_ => CLOSED),
    reqs: Map(),
    resps: Map(),
    nextId: 1,
  }

  pure def inFlight(s: State): int = s.reqs.keys().size() + s.resps.keys().size()

  pure def setClient(s: State, c: int, cl: Client): State = { ...s, clients: s.clients.set(c, cl) }
  pure def setSocket(s: State, c: int, so: Socket): State = { ...s, sockets: s.sockets.set(c, so) }

  // The host commits one event; every open socket subscribed to its stream gets a
  // `stream_advanced` (notifications are published after `COMMIT`).
  pure def appendEv(s: State, x: int, kind: Kind, sz: int): Out =
    val h = s.host
    val evs = logOf(h, h.epoch, x)
    val ev = { epoch: h.epoch, stream: x, seq: evs.length() + 1, size: sz, kind: kind }
    val h2 = { ...h, log: h.log.set(h.epoch, h.log.get(h.epoch).set(x, evs.append(ev))) }
    val socks = s.sockets.keys().mapBy(c => {
      val so = s.sockets.get(c)
      if (so.open and so.subs.contains(x)) { ...so, queue: so.queue.append(Advanced({ stream: x, head: ev.seq })) } else so
    })
    start({ ...s, host: h2, sockets: socks })

  // A daemon restart drops every socket and every request in flight; responses already
  // sent may still arrive.
  pure def restartHost(s: State): State = { ...s, sockets: CLIENTS.mapBy(_ => CLOSED), reqs: Map() }

  // A new database: a new epoch with empty streams.
  pure def resetDb(s: State): State =
    val e = s.host.epoch + 1
    restartHost({ ...s, host: { epoch: e, log: s.host.log.put(e, STREAMS.mapBy(_ => [])) } })

  // Every request issued so far becomes of an older generation for these streams.
  pure def resetStreams(cl: Client, xs: Set[int], nextId: int): Client = {
    ...cl,
    fwd: STREAMS.mapBy(x => if (xs.contains(x)) 0 else cl.fwd.get(x)),
    bwd: STREAMS.mapBy(x => if (xs.contains(x)) 0 else cl.bwd.get(x)),
    resetAt: STREAMS.mapBy(x => if (xs.contains(x)) nextId - 1 else cl.resetAt.get(x)),
  }

  /// Reconnect (mvp §2.4 "Live notifications"): a new socket, a `subscribe` of the
  /// streams the client follows, and a refetch of each (the client refetches on every
  /// reconnect); the requests of the previous connection are of an older generation.
  pure def connect(s: State, c: int): Out =
    val cl = s.clients.get(c)
    val cl2 = { ...resetStreams(cl, STREAMS, s.nextId), due: STREAMS.mapBy(x => cl.want.contains(x)) }
    start(setSocket(setClient(s, c, cl2), c, { ...CLOSED, open: true, pending: cl.want, hasPending: true }))

  pure def disconnect(s: State, c: int): Out = start(setSocket(s, c, CLOSED))

  /// A `subscribe` replaces the set; one not yet answered is replaced (subscribes may
  /// coalesce, http-surface §8 D15). The client drops the copies of streams it no longer
  /// follows and fetches the new ones.
  pure def resubscribe(s: State, c: int, w: Set[int]): Out =
    val cl = s.clients.get(c)
    val gone = STREAMS.filter(x => not(w.contains(x)))
    val cl1 = resetStreams(cl, gone, s.nextId)
    val cl2 = {
      ...cl1,
      want: w,
      copies: STREAMS.mapBy(x => if (gone.contains(x)) EMPTY_COPY else cl.copies.get(x)),
      due: STREAMS.mapBy(x => w.contains(x) and (cl.due.get(x) or not(cl.want.contains(x)))),
    }
    val so = s.sockets.get(c)
    val so2 = if (so.open) { ...so, pending: w, hasPending: true } else so
    start(setSocket(setClient(s, c, cl2), c, so2))

  pure def issue(s: State, c: int, x: int, dir: Dir, q: Query): Out =
    val id = s.nextId
    val cl = s.clients.get(c)
    val cl2 = match dir {
      | Fwd => { ...cl, fwd: cl.fwd.set(x, id), lastFwd: cl.lastFwd.set(x, id), due: cl.due.set(x, false) }
      | Bwd => { ...cl, bwd: cl.bwd.set(x, id), lastBwd: cl.lastBwd.set(x, id) }
    }
    val r = { id: id, client: c, stream: x, dir: dir, q: q }
    start({ ...setClient(s, c, cl2), reqs: s.reqs.put(id, r), nextId: id + 1 })

  /// A forward fetch from the cursor, with the epoch the client holds (omitted before the
  /// first response).
  pure def canFetch(s: State, c: int, x: int): bool =
    val cl = s.clients.get(c)
    cl.want.contains(x) and cl.fwd.get(x) == 0
  pure def fetchQuery(cl: Client, x: int, limit: int): Query =
    { after: cl.copies.get(x).hi, before: NONE, limit: limit, epoch: cl.epoch }
  pure def fetch(s: State, c: int, x: int, limit: int): Out =
    issue(s, c, x, Fwd, fetchQuery(s.clients.get(c), x, limit))

  /// A backward page before the oldest event held (mvp §2.4 "Backward history").
  pure def canFetchBack(s: State, c: int, x: int): bool =
    val cl = s.clients.get(c)
    val cp = cl.copies.get(x)
    and { cl.want.contains(x), cl.bwd.get(x) == 0, cp.epoch != 0, loOf(cp) > 1 }
  pure def fetchBack(s: State, c: int, x: int, limit: int): Out =
    val cl = s.clients.get(c)
    issue(s, c, x, Bwd, { after: NONE, before: loOf(cl.copies.get(x)), limit: limit, epoch: cl.epoch })

  /// The client gives up on a request (its connection failed or timed out); a forward
  /// fetch is then due again.
  pure def timeout(s: State, c: int, x: int, dir: Dir): Out =
    val cl = s.clients.get(c)
    val cl2 = match dir {
      | Fwd => { ...cl, fwd: cl.fwd.set(x, 0), due: cl.due.set(x, cl.want.contains(x)) }
      | Bwd => { ...cl, bwd: cl.bwd.set(x, 0) }
    }
    start(setClient(s, c, cl2))

  /// `GET /api/events`: the head, the decision, and the page or snapshot from one read
  /// snapshot of the current database (mvp §2.4; store §2.7).
  pure def serve(s: State, id: int): Out =
    val r = s.reqs.get(id)
    val h = s.host
    val evs = logOf(h, h.epoch, r.stream)
    val hd = evs.length()
    val after = if (r.q.after == NONE) 0 else r.q.after
    val sh = { headSeq: hd, cumHead: cumAt(evs, hd), cumAfter: if (after > hd) 0 else cumAt(evs, after) }
    val plan = decideReplay(r.q, sh, h.epoch)
    val body = match plan {
      | PForward(f) => {
          val n = min2(hd, f.after + f.limit)
          Page({ events: evs.slice(f.after, n), headSeq: hd, hasMore: hd > n, hasMoreBefore: f.after >= 1 })
        }
      | PBackward(b) => {
          val top = min2(b.before - 1, hd)
          val bottom = max2(1, top - b.limit + 1)
          Page({
            events: if (top >= 1) evs.slice(bottom - 1, top) else [],
            headSeq: hd,
            hasMore: hd >= max2(b.before, 1),
            hasMoreBefore: top >= 1 and bottom > 1,
          })
        }
      | PSnapshot(reason) => Snap({
          asOf: hd,
          recent: evs.slice(max2(0, hd - SNAPSHOT_RECENT_EVENTS), hd),
          proj: projOf(evs),
          threadStatus: if (r.stream == HOST) projOf(logOf(h, h.epoch, THREAD)).status else NONE,
          reason: reason,
        })
      | PRefused(e) => BadQuery(e)
    }
    {
      st: { ...s, reqs: without(s.reqs, id), resps: s.resps.put(id, { req: r, epoch: h.epoch, body: body }) },
      calls: [DecideReplay({ q: r.q, head: sh, epoch: h.epoch, plan: plan })],
    }

  pure def appendEvs(cp: Copy, e: int, evs: List[Ev]): Copy = {
    ...cp,
    epoch: e,
    events: cp.events.concat(evs),
    hi: if (evs.length() == 0) cp.hi else evs[evs.length() - 1].seq,
    proj: evs.foldl(cp.proj, applyEv),
  }

  /// Drops every local copy and takes the new epoch; every request issued so far is of
  /// an older generation, and each followed stream is fetched again.
  pure def switchEpoch(cl: Client, e: int, nextId: int): Client = {
    ...resetStreams(cl, STREAMS, nextId),
    epoch: e,
    copies: STREAMS.mapBy(_ => EMPTY_COPY),
    due: STREAMS.mapBy(x => cl.want.contains(x)),
  }

  pure def isCurrent(cl: Client, r: Req): bool = match r.dir {
    | Fwd => cl.fwd.get(r.stream) == r.id
    | Bwd => cl.bwd.get(r.stream) == r.id
  }

  /// The client applies a response (mvp §2.4 "Replay", "Backward history"): one of an
  /// older generation changes nothing; a forward response with another epoch switches
  /// epoch and local state together; a backward page with another epoch only makes a
  /// forward resync due.
  pure def deliver(s: State, id: int): Out =
    val p = s.resps.get(id)
    val c = p.req.client
    val x = p.req.stream
    val cl = s.clients.get(c)
    val s0 = { ...s, resps: without(s.resps, id) }
    if (not(isCurrent(cl, p.req))) start(s0)
    else {
      val cl1 = match p.req.dir {
        | Fwd => { ...cl, fwd: cl.fwd.set(x, 0) }
        | Bwd => { ...cl, bwd: cl.bwd.set(x, 0) }
      }
      val cl2 = match p.body {
        | BadQuery(_) => cl1
        | Page(pg) => match p.req.dir {
            | Fwd =>
              if (cl1.epoch != 0 and p.epoch != cl1.epoch) {
                // Only a page from the start can be the new epoch's local state.
                val sw = switchEpoch(cl1, p.epoch, s.nextId)
                if (p.req.q.after > 0) sw
                else { ...sw, copies: sw.copies.set(x, appendEvs(EMPTY_COPY, p.epoch, pg.events)), due: sw.due.set(x, pg.hasMore) }
              } else
                { ...cl1, epoch: p.epoch, copies: cl1.copies.set(x, appendEvs(cl1.copies.get(x), p.epoch, pg.events)), due: cl1.due.set(x, pg.hasMore) }
            | Bwd =>
              if (p.epoch != cl1.epoch) { ...cl1, due: cl1.due.set(x, true) }
              else {
                val cp = cl1.copies.get(x)
                { ...cl1, copies: cl1.copies.set(x, { ...cp, events: pg.events.concat(cp.events) }) }
              }
          }
        | Snap(sn) => {
            val cp = { epoch: p.epoch, events: sn.recent, hi: sn.asOf, proj: sn.proj, threadStatus: sn.threadStatus }
            // A snapshot replaces the stream's copy: a backward request built from the old
            // copy is of an older generation.
            val base = if (p.epoch != cl1.epoch) switchEpoch(cl1, p.epoch, s.nextId)
              else resetStreams(cl1, Set(x), s.nextId)
            { ...base, copies: base.copies.set(x, cp), due: base.due.set(x, false) }
          }
      }
      start(setClient(s0, c, cl2))
    }

  pure def headsOf(s: State, xs: Set[int]): int -> int = xs.mapBy(x => headOf(s.host, x))

  /// The server answers the latest subscribe with `heads` for the new set.
  pure def serverSubscribe(s: State, c: int): Out =
    val so = s.sockets.get(c)
    start(setSocket(s, c, { ...so, subs: so.pending, hasPending: false, queue: so.queue.append(Heads(headsOf(s, so.pending))) }))

  /// A heads refresh (due every 30 s): every subscribed stream with its current head.
  pure def headsRefresh(s: State, c: int): Out =
    val so = s.sockets.get(c)
    start(setSocket(s, c, { ...so, queue: so.queue.append(Heads(headsOf(s, so.subs))) }))

  pure def newerHead(cl: Client, x: int, h: int): bool = cl.want.contains(x) and h > cl.copies.get(x).hi

  /// Notify-then-pull: a head newer than the cursor makes a fetch due; a `heads` frame is
  /// a snapshot of every listed stream's head.
  pure def recvNote(s: State, c: int): Out =
    val so = s.sockets.get(c)
    val n = head(so.queue)
    val cl = s.clients.get(c)
    val due2 = match n {
      | Advanced(a) => if (newerHead(cl, a.stream, a.head)) cl.due.set(a.stream, true) else cl.due
      | Heads(m) => STREAMS.mapBy(x => cl.due.get(x) or (m.keys().contains(x) and newerHead(cl, x, m.get(x))))
    }
    start(setSocket(setClient(s, c, { ...cl, due: due2 }), c, { ...so, queue: tail(so.queue) }))

  // ---------------------------------------------------------------------------
  // Convergence: connectivity returns, writes stop, and the protocol runs on a
  // deterministic fair schedule (bounded liveness)
  // ---------------------------------------------------------------------------

  /// Some followed stream is behind the host in the same epoch, with no fetch due or in
  /// flight, no pending subscribe, and no pending notification newer than its cursor.
  pure def onlyHeadsRecover(s: State): bool = tuples(CLIENTS, STREAMS).exists(p => {
    val cl = s.clients.get(p._1)
    val so = s.sockets.get(p._1)
    val cp = cl.copies.get(p._2)
    and {
      cl.want.contains(p._2), cp.epoch == s.host.epoch, cp.hi < headOf(s.host, p._2),
      not(cl.due.get(p._2)), cl.fwd.get(p._2) == 0, so.open, not(so.hasPending),
      so.queue.indices().forall(i => match so.queue[i] {
        | Advanced(a) => not(a.stream == p._2 and a.head > cp.hi)
        | Heads(m) => not(m.keys().contains(p._2) and m.get(p._2) > cp.hi)
      }),
    }
  })

  pure def restore(s: State): Out =
    CLIENTS.fold(start(s), (o, c) => if (o.st.sockets.get(c).open) o else andThen(o, t => connect(t, c)))

  // `done`: the schedule reached its fixpoint; the remaining steps change nothing.
  type Drain = { o: Out, refreshes: int, done: bool }

  pure def dueStreams(s: State, c: int): Set[int] =
    val cl = s.clients.get(c)
    cl.want.filter(x => cl.due.get(x) and cl.fwd.get(x) == 0)

  pure def lostStreams(s: State, c: int): Set[int] =
    val cl = s.clients.get(c)
    STREAMS.filter(x => cl.fwd.get(x) != 0 and not(s.reqs.keys().contains(cl.fwd.get(x)) or s.resps.keys().contains(cl.fwd.get(x))))

  /// One step of a fixed schedule: responses, requests, subscribes, notifications, due
  /// fetches, timeouts of lost requests, then heads refreshes.
  pure def drainOne(d: Drain): Drain = if (d.done) d else {
    val s = d.o.st
    val subscribing = CLIENTS.filter(c => s.sockets.get(c).open and s.sockets.get(c).hasPending)
    val notified = CLIENTS.filter(c => s.sockets.get(c).open and s.sockets.get(c).queue.length() > 0)
    val fetching = CLIENTS.filter(c => dueStreams(s, c).size() > 0)
    val stuck = CLIENTS.filter(c => lostStreams(s, c).size() > 0)
    if (s.resps.keys().size() > 0) { ...d, o: andThen(d.o, t => deliver(t, minOf(t.resps.keys()))) }
    else if (s.reqs.keys().size() > 0) { ...d, o: andThen(d.o, t => serve(t, minOf(t.reqs.keys()))) }
    else if (subscribing.size() > 0) { ...d, o: andThen(d.o, t => serverSubscribe(t, minOf(subscribing))) }
    else if (notified.size() > 0) { ...d, o: andThen(d.o, t => recvNote(t, minOf(notified))) }
    else if (fetching.size() > 0) {
      val c = minOf(fetching)
      { ...d, o: andThen(d.o, t => fetch(t, c, minOf(dueStreams(t, c)), 2)) }
    } else if (stuck.size() > 0) {
      val c = minOf(stuck)
      { ...d, o: andThen(d.o, t => timeout(t, c, minOf(lostStreams(t, c)), Fwd)) }
    } else if (d.refreshes > 0) {
      val open = CLIENTS.filter(c => s.sockets.get(c).open)
      { ...d, o: open.fold(d.o, (o, c) => andThen(o, t => headsRefresh(t, c))), refreshes: d.refreshes - 1 }
    } else { ...d, done: true }
  }

  pure def drain(s: State): Out =
    range(0, DRAIN_STEPS).foldl({ o: restore(s), refreshes: DRAIN_REFRESHES, done: false }, (d, _) => drainOne(d)).o

  /// Every client has, for each stream it follows, the host's epoch, cursor, and
  /// rendered state.
  pure def converged(s: State): bool = CLIENTS.forall(c => {
    val cl = s.clients.get(c)
    cl.want.forall(x => {
      val cp = cl.copies.get(x)
      val evs = logOf(s.host, s.host.epoch, x)
      and { cl.epoch == s.host.epoch, cp.epoch == s.host.epoch, cp.hi == evs.length(), cp.proj == projOf(evs) }
    })
  })

  // ---------------------------------------------------------------------------
  // Actions
  // ---------------------------------------------------------------------------

  action commit(a: Action, o: Out): bool = all {
    st' = o.st,
    last' = { act: a, calls: o.calls, prevClients: st.clients },
  }

  action initWith(hostEvents: int, threadEvents: int, big: bool, want2: Set[int]): bool = all {
    val s = initialState(hostEvents, threadEvents, big, want2)
    all {
      st' = s,
      last' = {
        act: Init({
          hostEvents: hostEvents, threadEvents: threadEvents, big: big, want2: want2,
          replayMaxEvents: REPLAY_MAX_EVENTS, replayMaxBytes: REPLAY_MAX_BYTES,
        }),
        calls: [],
        prevClients: s.clients,
      },
    },
  }

  action init = {
    nondet h = 0.to(3).oneOf()
    nondet t = 0.to(3).oneOf()
    nondet big = Bool.oneOf()
    nondet w = Set(Set(HOST), Set(THREAD), STREAMS).oneOf()
    initWith(h, t, big, w)
  }

  action appendA(x: int, kind: Kind, sz: int): bool = all {
    headOf(st.host, x) < MAX_EVENTS,
    commit(Append({ stream: x, kind: kind, size: sz }), appendEv(st, x, kind, sz)),
  }

  action restart: bool = commit(Restart, start(restartHost(st)))

  action reset: bool = all {
    st.host.epoch < MAX_EPOCHS,
    commit(ResetDb, start(resetDb(st))),
  }

  action connectA(c: int): bool = all {
    not(st.sockets.get(c).open),
    commit(Connect(c), connect(st, c)),
  }

  action disconnectA(c: int): bool = all {
    st.sockets.get(c).open,
    commit(Disconnect(c), disconnect(st, c)),
  }

  action resubscribeA(c: int, w: Set[int]): bool = all {
    w != Set(),
    w != st.clients.get(c).want,
    commit(Resubscribe({ client: c, want: w, replacedPending: st.sockets.get(c).open and st.sockets.get(c).hasPending }), resubscribe(st, c, w)),
  }

  action fetchA(c: int, x: int, limit: int): bool = all {
    canFetch(st, c, x),
    inFlight(st) < MAX_IN_FLIGHT,
    val o = fetch(st, c, x, limit)
    commit(Fetch(o.st.reqs.get(st.nextId)), o),
  }

  action fetchBackA(c: int, x: int, limit: int): bool = all {
    canFetchBack(st, c, x),
    inFlight(st) < MAX_IN_FLIGHT,
    val o = fetchBack(st, c, x, limit)
    commit(FetchBack(o.st.reqs.get(st.nextId)), o),
  }

  /// A forward request with any query (replay coverage of refusals and `CursorAhead`).
  action rawFetch(c: int, x: int, q: Query): bool = all {
    canFetch(st, c, x),
    val o = issue(st, c, x, Fwd, q)
    commit(RawFetch(o.st.reqs.get(st.nextId)), o),
  }

  action timeoutA(c: int, x: int, dir: Dir): bool = all {
    match dir { | Fwd => st.clients.get(c).fwd.get(x) != 0 | Bwd => st.clients.get(c).bwd.get(x) != 0 },
    commit(Timeout({ client: c, stream: x, dir: dir }), timeout(st, c, x, dir)),
  }

  action serveA(id: int): bool = all {
    st.reqs.keys().contains(id),
    commit(Serve(st.reqs.get(id)), serve(st, id)),
  }

  action loseReq(id: int): bool = all {
    st.reqs.keys().contains(id),
    commit(LoseReq(st.reqs.get(id)), start({ ...st, reqs: without(st.reqs, id) })),
  }

  action loseResp(id: int): bool = all {
    st.resps.keys().contains(id),
    commit(LoseResp(st.resps.get(id).req), start({ ...st, resps: without(st.resps, id) })),
  }

  action deliverA(id: int): bool = all {
    st.resps.keys().contains(id),
    commit(Deliver(st.resps.get(id).req), deliver(st, id)),
  }

  action serverSubscribeA(c: int): bool = all {
    st.sockets.get(c).open,
    st.sockets.get(c).hasPending,
    commit(ServerSubscribe(c), serverSubscribe(st, c)),
  }

  action headsRefreshA(c: int): bool = all {
    st.sockets.get(c).open,
    commit(HeadsRefresh(c), headsRefresh(st, c)),
  }

  action recvNoteA(c: int): bool = all {
    st.sockets.get(c).open,
    st.sockets.get(c).queue.length() > 0,
    commit(RecvNote({ client: c, note: head(st.sockets.get(c).queue) }), recvNote(st, c)),
  }

  /// A lost notification: the socket's receiver lagged or a frame was coalesced away.
  action dropNote(c: int, i: int): bool = all {
    val q = st.sockets.get(c).queue
    all {
      i < q.length(),
      commit(DropNote({ client: c, note: q[i] }), start({
        ...setSocket(st, c, { ...st.sockets.get(c), queue: q.slice(0, i).concat(q.slice(i + 1, q.length())) }),
      })),
    },
  }

  /// Connectivity returns and writes stop: the protocol runs to its fixpoint on a fair
  /// deterministic schedule (`drain`).
  action quiesce: bool = commit(Quiesce({ onlyHeads: onlyHeadsRecover(restore(st).st) }), drain(st))

  pure def someId(ids: Set[int]): Set[int] = if (ids.size() == 0) Set(0) else ids

  action networkStep = any {
    nondet id = someId(st.reqs.keys()).oneOf()
    any { serveA(id), serveA(id), loseReq(id) },
    nondet id = someId(st.resps.keys()).oneOf()
    any { deliverA(id), deliverA(id), loseResp(id) },
    nondet c = CLIENTS.oneOf()
    any { serverSubscribeA(c), recvNoteA(c), recvNoteA(c), dropNote(c, 0), headsRefreshA(c) },
  }

  action clientStep = {
    nondet c = CLIENTS.oneOf()
    nondet x = STREAMS.oneOf()
    nondet limit = Set(1, 2).oneOf()
    any { fetchA(c, x, limit), fetchA(c, x, limit), fetchBackA(c, x, limit) }
  }

  action hostStep = any {
    nondet x = STREAMS.oneOf()
    nondet k = Set(Plain, SurfaceUpdate, StatusChange).oneOf()
    nondet sz = Set(1, 3).oneOf()
    appendA(x, k, sz),
    restart,
    reset,
  }

  action disturbanceStep = {
    nondet c = CLIENTS.oneOf()
    nondet x = STREAMS.oneOf()
    nondet dir = Set(Fwd, Bwd).oneOf()
    nondet w = Set(Set(HOST), Set(THREAD), STREAMS).oneOf()
    any { connectA(c), connectA(c), disconnectA(c), resubscribeA(c, w), timeoutA(c, x, dir) }
  }

  // Progress (the network and the client) has twice the weight of the host's writes and
  // of disturbances.
  action step = any { networkStep, networkStep, clientStep, clientStep, hostStep, disturbanceStep, quiesce }

  // ---------------------------------------------------------------------------
  // Invariants (each cites its source)
  // ---------------------------------------------------------------------------

  pure def copiesOf(s: State): Set[{ c: int, x: int, cp: Copy }] =
    tuples(CLIENTS, STREAMS).map(p => { c: p._1, x: p._2, cp: s.clients.get(p._1).copies.get(p._2) })

  /// mvp §2.4 (CL-13 "without duplicates"; event-store CL-9 gapless `seq`): a local copy
  /// holds consecutive events ending at its cursor, none twice, and none missing between
  /// a snapshot's recent events and the events after `as_of_seq`.
  val noGapNoDup = copiesOf(st).forall(r => {
    val evs = r.cp.events
    val n = evs.length()
    and {
      evs.indices().forall(i => i == 0 or evs[i].seq == evs[i - 1].seq + 1),
      n == 0 or evs[n - 1].seq == r.cp.hi,
      r.cp.epoch == 0 or r.cp.hi == 0 or n > 0,
    }
  })

  /// mvp §2.4 (CL-13 "end with the server's events"): every event a client holds is the
  /// host's event at that position of the epoch the client holds, and its cursor is not
  /// beyond that epoch's head.
  val heldEventsOnHost = copiesOf(st).forall(r => r.cp.epoch == 0 or {
    val evs = logOf(st.host, r.cp.epoch, r.x)
    and {
      r.cp.hi <= evs.length(),
      r.cp.events.indices().forall(i => {
        val e = r.cp.events[i]
        e.seq >= 1 and e.seq <= evs.length() and evs[e.seq - 1] == e
      }),
    }
  })

  /// mvp §2.4 "switching epoch and local state together": a client's local state is of
  /// one epoch, the one it holds; no copy keeps state or events of another.
  val noMixedEpochs = copiesOf(st).forall(r => {
    val held = st.clients.get(r.c).epoch
    and { r.cp.epoch == 0 or r.cp.epoch == held, r.cp.events.indices().forall(i => r.cp.events[i].epoch == held) }
  })

  /// mvp §2.4 "Everything a client renders is in a snapshot or in events after
  /// `as_of_seq`": what a copy renders is the host's projection at the copy's cursor.
  val renderedStateAtCursor = copiesOf(st).forall(r => r.cp.epoch == 0 or
    r.cp.proj == projOf(logOf(st.host, r.cp.epoch, r.x).slice(0, r.cp.hi)))

  // A response is of an older generation when its request was issued before the client
  // last replaced that stream's copy, switched epoch, or reconnected, or when the client
  // issued a later request for the same stream and direction (recorded ghosts).
  pure def older(cl: Client, r: Req): bool =
    val latest = match r.dir { | Fwd => cl.lastFwd.get(r.stream) | Bwd => cl.lastBwd.get(r.stream) }
    r.id <= cl.resetAt.get(r.stream) or r.id < latest

  /// mvp §2.4 "a response from an older request generation is discarded".
  val olderResponsesChangeNothing = match last.act {
    | Deliver(r) => older(last.prevClients.get(r.client), r) implies st.clients.get(r.client) == last.prevClients.get(r.client)
    | _ => true
  }

  /// mvp §2.4 "only a forward response (on the active reconnect or fetch) may establish a
  /// new epoch": a client never goes back to an epoch the host left. Epochs are random ids
  /// in the contract; the model numbers them in creation order to state this.
  val epochNeverRegresses = CLIENTS.forall(c => st.clients.get(c).epoch >= last.prevClients.get(c).epoch)

  /// mvp §2.4 "Live notifications", CL-13, http-surface §8 D15: after connectivity
  /// returns and writes stop, every client converges to the host's events and rendered
  /// state of each stream it follows within DRAIN_STEPS steps of a fair schedule, despite
  /// lost responses and notifications, disconnects, restarts, and epoch changes before it.
  val convergedAfterQuiesce = match last.act {
    | Quiesce(_) => converged(st)
    | _ => true
  }

  val allInvariants = and {
    noGapNoDup,
    heldEventsOnHost,
    noMixedEpochs,
    renderedStateAtCursor,
    olderResponsesChangeNothing,
    epochNeverRegresses,
    convergedAfterQuiesce,
  }

  // Witnesses: reachability of the interesting states (`--witnesses`; `simulate` fails
  // when one is never reached).
  pure def planned(cs: List[Call], p: Plan): bool = cs.indices().exists(i => match cs[i] { | DecideReplay(d) => d.plan == p })
  val witnessEpochSwitch = match last.act {
    | Deliver(r) => last.prevClients.get(r.client).epoch != 0 and st.clients.get(r.client).epoch != last.prevClients.get(r.client).epoch
    | _ => false
  }
  val witnessConvergedAcrossEpochs = match last.act {
    | Quiesce(_) => CLIENTS.exists(c => last.prevClients.get(c).epoch != 0 and last.prevClients.get(c).epoch < st.host.epoch) and converged(st)
    | _ => false
  }
  val witnessOlderDiscarded = match last.act {
    | Deliver(r) => older(last.prevClients.get(r.client), r) and r.dir == Fwd
    | _ => false
  }
  val witnessBackwardApplied = match last.act {
    | Deliver(r) => r.dir == Bwd and
        st.clients.get(r.client).copies.get(r.stream).events.length() > last.prevClients.get(r.client).copies.get(r.stream).events.length()
    | _ => false
  }
  val witnessBackwardEpochMismatch = match last.act {
    | Deliver(r) => r.dir == Bwd and not(older(last.prevClients.get(r.client), r)) and
        st.clients.get(r.client).epoch != st.host.epoch and st.clients.get(r.client).due.get(r.stream)
    | _ => false
  }
  val witnessTooManyEvents = planned(last.calls, PSnapshot(TooManyEvents))
  val witnessTooManyBytes = planned(last.calls, PSnapshot(TooManyBytes))
  val witnessEpochChanged = planned(last.calls, PSnapshot(EpochChanged))
  val witnessHasMore = match last.act {
    | Deliver(r) => r.dir == Fwd and not(older(last.prevClients.get(r.client), r)) and st.clients.get(r.client).due.get(r.stream)
        and st.clients.get(r.client).epoch == last.prevClients.get(r.client).epoch
    | _ => false
  }
  val witnessOnlyHeadsRecover = match last.act {
    | Quiesce(q) => q.onlyHeads and converged(st)
    | _ => false
  }
  val witnessCoalescedSubscribe = match last.act {
    | Resubscribe(r) => r.replacedPending
    | _ => false
  }
}
