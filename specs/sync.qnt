// -*- mode: Bluespec; -*-
/**
 * Q-B, read side: the Sync Seam protocol between one authoritative host and PWA-like
 * clients (docs/mvp-design.md §2.4 "Replay", "Backward history", "Live notifications";
 * docs/design/http-surface.md §2.6, §2.7; docs/design/formal-specs.md §7).
 *
 * Two layers, as in Q-A:
 *
 * - `decideReplay` mirrors `crates/core/src/sync.rs` `decide_replay` branch for branch,
 *   with its event and byte bounds scaled down (one model seq stands for 5 000 real ones,
 *   one size unit for 2 MiB; every comparison in `decide_replay` is linear, so scaling
 *   keeps each branch). Every step records its calls (`last.calls`), and the committed
 *   traces replay against the real function (crates/core/tests/quint_sync_traces.rs).
 * - The protocol layer is the contract: the host answers each request from one read
 *   snapshot (PR 6's store guarantees are assumed: atomic commit, gapless per-stream
 *   `seq`, the epoch fixed at database creation); each socket merges heads into `known`
 *   and builds its frames at send selection (http §2.6); the network loses, delays, and
 *   reorders responses and loses frames; the client keeps one epoch, a local copy per
 *   stream, the heads it observed, and request generations (mvp §2.4, "A client keeps one
 *   current request per stream and direction").
 *
 * Abstracted: the host is one database with an epoch; a new database (a new epoch) starts
 * with empty streams, and a thread may not exist in it; events carry their identity
 * (epoch, stream, seq) as a ghost; the rendered state of a stream is a projection of its
 * events (`Proj`: the latest surface and a status count); time is explicit actions (a
 * heads refresh, a client's request timeout, a read that fails with 408 or 503).
 * Not modelled: authorization (a stream the token may not read is the same 404 as a
 * missing one), blobs, tickets, the socket's read and send deadlines (a socket that
 * misses them is a `disconnect`), token revocation.
 */
module sync {
  import common.* from "./common"

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
  // The bounded-liveness check: deterministic steps after connectivity returns.
  pure val DRAIN_STEPS = 60
  pure val DRAIN_REFRESHES = 2
  // An omitted query parameter.
  pure val NONE = -1

  pure def min2(a: int, b: int): int = if (a < b) a else b
  pure def max2(a: int, b: int): int = if (a > b) a else b
  pure def minOf(s: Set[int]): int = s.fold(1000000, (a, b) => min2(a, b))
  pure def seqsOf(evs: List[Ev]): Set[int] = evs.indices().map(i => evs[i].seq)

  // ---------------------------------------------------------------------------
  // Reducer layer: crates/core/src/sync.rs `decide_replay`
  // ---------------------------------------------------------------------------

  pure val LIMIT_MIN = 1
  pure val LIMIT_MAX = 1000
  pure val LIMIT_DEFAULT = 200
  // Scaled bounds (see the module comment); the replay test scales back.
  pure val REPLAY_MAX_EVENTS = 2
  pure val REPLAY_MAX_BYTES = 4
  // Events a thread snapshot carries (`SNAPSHOT_RECENT_EVENTS`, 50 in the code); a host
  // snapshot carries none (`HostSnapshot`).
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

  // `log` keeps every epoch's events: a client may still hold an older epoch's. `streams`
  // are the streams each epoch's database has: the host stream always, the thread only if
  // that database registered it.
  type Host = { epoch: int, log: int -> (int -> List[Ev]), streams: int -> Set[int] }

  pure def logOf(h: Host, e: int, s: int): List[Ev] = h.log.get(e).get(s)
  pure def headOf(h: Host, s: int): int = logOf(h, h.epoch, s).length()
  pure def hasStream(h: Host, s: int): bool = h.streams.get(h.epoch).contains(s)

  // ---------------------------------------------------------------------------
  // Network, sockets, and client
  // ---------------------------------------------------------------------------

  type Dir = Fwd | Bwd
  type Req = { id: int, client: int, stream: int, dir: Dir, q: Query }
  type Body =
    | Page({ events: List[Ev], hasMore: bool, hasMoreBefore: bool })
    // `recent`: none for the host stream. `threadStatus`: in a host snapshot, the thread's
    // status read in the same snapshot (`ThreadInfo`, a field of another stream); NONE in a
    // thread snapshot.
    | Snap({ asOf: int, recent: List[Ev], proj: Proj, threadStatus: int })
    // 400: `decide_replay` refused the query (the recorded call holds the error).
    | BadQuery
    // 404: the stream does not exist in the current database.
    | NotFound
    // A read that failed (408 at its cutoff, 503 `busy`): retryable, no state.
    | ReadFailed
  type Resp = { req: Req, epoch: int, body: Body }

  type Note = Advanced({ stream: int, head: int }) | Heads(int -> int)

  // The server's side of one client's socket (http §2.6): its database's epoch (`hello`),
  // the subscription set `S`, the latest subscribe not yet answered (a later one replaces
  // it), `known` and `announced` heads per stream of `S`, a heads refresh due, and the one
  // frame in flight. Frames are built at send selection from `known`.
  type Frame = NoFrame | InFlight(Note)
  type Socket = {
    open: bool,
    subs: Set[int],
    pending: Set[int],
    hasPending: bool,
    known: int -> int,
    announced: int -> int,
    headsDue: bool,
    frame: Frame,
  }
  pure val CLOSED: Socket = {
    open: false, subs: Set(), pending: Set(), hasPending: false,
    known: Map(), announced: Map(), headsDue: false, frame: NoFrame,
  }

  /// The frame in flight; callers check that there is one (`Heads(Map())` otherwise).
  pure def frameNote(so: Socket): Note = match so.frame { | InFlight(n) => n | NoFrame => Heads(Map()) }

  /// The streams the frame in flight names (none without one).
  pure def frameNames(so: Socket): Set[int] = match so.frame { | InFlight(n) => noteNames(n) | NoFrame => Set() }

  // A client's local copy of one stream. `epoch` 0: no local state. `hi` is the cursor:
  // everything up to it is known (from a snapshot or events); `events` are the envelopes
  // held, consecutive and ending at `hi` when there are any (a host snapshot brings none).
  // `moreBefore`: older events exist (`has_more_before`, or below a snapshot's events).
  // `threadStatus`: from the last host snapshot (host stream only), an initial value; the
  // client renders the thread's status from the thread stream (formal-specs §7.4 F1).
  type Copy = { epoch: int, events: List[Ev], hi: int, proj: Proj, threadStatus: int, moreBefore: bool }
  pure val EMPTY_COPY: Copy = { epoch: 0, events: [], hi: 0, proj: NO_PROJ, threadStatus: NONE, moreBefore: false }

  pure def loOf(cp: Copy): int = if (cp.events.length() == 0) cp.hi + 1 else cp.events[0].seq

  // The client. `fwd` and `bwd` name the current request per stream and direction (0:
  // none). `need`: a forward fetch is due for a reason of its own (reconnect, epoch switch,
  // a resync, a failed or timed-out fetch, a newly followed stream); `more`: the last page
  // had `has_more`; `seen`: the newest head observed on the socket of epoch `sockEpoch`.
  // A stream is due while any of them says so (`dueOf`), so a response never hides a
  // notification that arrived during its fetch.
  // Ghosts for the invariants, updated where the events happen: `lastFwd`, `lastBwd` (the
  // latest request issued), `resetAt` (requests up to this id are of an older generation),
  // `cancelled` (requests given up), `lost` (streams a lost frame named since connecting),
  // `owed` (streams whose resync was asked for after their forward request was issued: a
  // backward page of another epoch or a failed forward read; a forward request issued
  // later pays it).
  type Client = {
    epoch: int,
    copies: int -> Copy,
    fwd: int -> int,
    bwd: int -> int,
    need: int -> bool,
    more: int -> bool,
    seen: int -> int,
    sockEpoch: int,
    want: Set[int],
    lastFwd: int -> int,
    lastBwd: int -> int,
    resetAt: int -> int,
    cancelled: Set[int],
    lost: Set[int],
    owed: Set[int],
  }

  /// F1: the thread status a client shows: from the thread stream once it follows it and
  /// holds its copy; until then, the value a host snapshot read at its `as_of_seq`.
  pure def shownThreadStatus(cl: Client): int =
    if (cl.want.contains(THREAD) and cl.copies.get(THREAD).epoch != 0) cl.copies.get(THREAD).proj.status
    else cl.copies.get(HOST).threadStatus

  pure def zeros(x: int): int = 0
  pure def falses(x: int): bool = false

  pure def newClient(want: Set[int]): Client = {
    epoch: 0,
    copies: STREAMS.mapBy(_ => EMPTY_COPY),
    fwd: STREAMS.mapBy(zeros),
    bwd: STREAMS.mapBy(zeros),
    need: STREAMS.mapBy(x => want.contains(x)),
    more: STREAMS.mapBy(falses),
    seen: STREAMS.mapBy(zeros),
    sockEpoch: 0,
    want: want,
    lastFwd: STREAMS.mapBy(zeros),
    lastBwd: STREAMS.mapBy(zeros),
    resetAt: STREAMS.mapBy(zeros),
    cancelled: Set(),
    lost: Set(),
    owed: Set(),
  }

  pure def dueOf(cl: Client, x: int): bool = and {
    cl.want.contains(x),
    cl.fwd.get(x) == 0,
    or {
      cl.need.get(x),
      cl.more.get(x),
      cl.sockEpoch == cl.epoch and cl.seen.get(x) > cl.copies.get(x).hi,
    },
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
    // `connected`: both clients start with an open socket whose subscribe was answered.
    // `racing`: the start of a database swap race (`racingState`).
    | Init({
        hostEvents: int, threadEvents: int, big: bool, want2: Set[int], connected: bool, racing: bool,
        replayMaxEvents: int, replayMaxBytes: int,
      })
    | Append({ stream: int, kind: Kind, size: int })
    | Restart
    // `threadKept`: the new database has the thread too.
    | ResetDb({ threadKept: bool })
    | Connect(int)
    | Disconnect(int)
    // `replacedPending`: the socket still held an unanswered subscribe.
    | Resubscribe({ client: int, want: Set[int], replacedPending: bool })
    | Fetch(Req)
    | FetchBack(Req)
    | RawFetch(Req)
    | Timeout({ client: int, stream: int, dir: Dir })
    | Serve(Req)
    | FailRead(Req)
    | LoseReq(Req)
    | LoseResp(Req)
    | Deliver(Resp)
    | ServerSubscribe(int)
    | HeadsRefresh(int)
    | SendFrame({ client: int, note: Note })
    | RecvNote({ client: int, note: Note })
    | DropNote({ client: int, note: Note })
    // `onlyHeads`: once connectivity returned, some followed stream was behind the host
    // after a lost frame, with no fetch due and no frame for it to come: only a heads
    // refresh recovers it.
    | Quiesce({ onlyHeads: bool })

  // The results of the invariants over the steps inside one `quiesce` (all true for any
  // other action); each invariant also reads its own field.
  type Checks = {
    gap: bool, held: bool, mixed: bool, rendered: bool, kept: bool,
    older: bool, regress: bool, establish: bool, owed: bool,
  }
  pure val ALL_OK: Checks = {
    gap: true, held: true, mixed: true, rendered: true, kept: true,
    older: true, regress: true, establish: true, owed: true,
  }

  type Last = { act: Action, calls: List[Call], prevClients: int -> Client, inner: Checks }

  var st: State
  var last: Last

  // ---------------------------------------------------------------------------
  // Transitions as pure functions (the actions and the drain share them)
  // ---------------------------------------------------------------------------

  type Out = { st: State, calls: List[Call] }

  pure def start(s: State): Out = { st: s, calls: [] }

  pure def kindOf(seq: int): Kind = if (seq == 2) SurfaceUpdate else if (seq == 3) StatusChange else Plain

  pure def initialLog(e: int, hostEvents: int, threadEvents: int, sz: int): int -> List[Ev] =
    STREAMS.mapBy(x => {
      val n = if (x == HOST) hostEvents else threadEvents
      range(1, n + 1).foldl([], (l, k) => l.append({ epoch: e, stream: x, seq: k, size: sz, kind: kindOf(k) }))
    })

  pure def initialState(hostEvents: int, threadEvents: int, big: bool, want2: Set[int]): State = {
    host: {
      epoch: 1,
      log: Map(1 -> initialLog(1, hostEvents, threadEvents, if (big) 3 else 1)),
      streams: Map(1 -> STREAMS),
    },
    clients: Map(1 -> newClient(STREAMS), 2 -> newClient(want2)),
    sockets: CLIENTS.mapBy(_ => CLOSED),
    reqs: Map(),
    resps: Map(),
    nextId: 1,
  }

  pure def inFlight(s: State): int = s.reqs.keys().size() + s.resps.keys().size()

  pure def setClient(s: State, c: int, cl: Client): State = { ...s, clients: s.clients.set(c, cl) }
  pure def setSocket(s: State, c: int, so: Socket): State = { ...s, sockets: s.sockets.set(c, so) }

  /// The host commits one event; every open socket subscribed to its stream merges the
  /// new head into `known` (notifications are published after `COMMIT`).
  pure def appendEv(s: State, x: int, kind: Kind, sz: int): Out =
    val h = s.host
    val evs = logOf(h, h.epoch, x)
    val ev = { epoch: h.epoch, stream: x, seq: evs.length() + 1, size: sz, kind: kind }
    val h2 = { ...h, log: h.log.set(h.epoch, h.log.get(h.epoch).set(x, evs.append(ev))) }
    val socks = s.sockets.keys().mapBy(c => {
      val so = s.sockets.get(c)
      if (so.open and so.subs.contains(x)) { ...so, known: so.known.set(x, ev.seq) } else so
    })
    start({ ...s, host: h2, sockets: socks })

  // A daemon restart drops every socket and every request in flight; responses already
  // sent may still arrive.
  pure def restartHost(s: State): State = { ...s, sockets: CLIENTS.mapBy(_ => CLOSED), reqs: Map() }

  /// A new database: a new epoch with empty streams; the thread exists in it or not.
  pure def resetDb(s: State, threadKept: bool): State =
    val e = s.host.epoch + 1
    restartHost({ ...s, host: {
      epoch: e,
      log: s.host.log.put(e, STREAMS.mapBy(_ => [])),
      streams: s.host.streams.put(e, if (threadKept) STREAMS else Set(HOST)),
    } })

  // The client's own bookkeeping: these streams' requests stop being current.
  pure def clearRequests(cl: Client, xs: Set[int]): Client = {
    ...cl,
    fwd: STREAMS.mapBy(x => if (xs.contains(x)) 0 else cl.fwd.get(x)),
    bwd: STREAMS.mapBy(x => if (xs.contains(x)) 0 else cl.bwd.get(x)),
  }

  // The ghost: every request issued so far is of an older generation for these streams.
  pure def markOlder(cl: Client, xs: Set[int], nextId: int): Client =
    { ...cl, resetAt: STREAMS.mapBy(x => if (xs.contains(x)) nextId - 1 else cl.resetAt.get(x)) }

  /// Reconnect (mvp §2.4 "Live notifications"): a new socket with its `hello` epoch, a
  /// `subscribe` of the streams the client follows, and a refetch of each (the client
  /// refetches on every reconnect). The requests of the previous connection are of an
  /// older generation; heads seen on the old socket are forgotten.
  pure def connect(s: State, c: int): Out =
    val cl = s.clients.get(c)
    val cl2 = {
      ...markOlder(clearRequests(cl, STREAMS), STREAMS, s.nextId),
      need: STREAMS.mapBy(x => cl.want.contains(x)),
      seen: STREAMS.mapBy(zeros),
      sockEpoch: s.host.epoch,
      lost: Set(),
    }
    start(setSocket(setClient(s, c, cl2), c, { ...CLOSED, open: true, pending: cl.want, hasPending: true }))

  pure def disconnect(s: State, c: int): Out = start(setSocket(s, c, CLOSED))

  // The client stops following these streams: their copies, requests, and seen heads go.
  pure def dropStreams(cl: Client, xs: Set[int], nextId: int): Client = {
    ...markOlder(clearRequests(cl, xs), xs, nextId),
    want: cl.want.exclude(xs),
    copies: STREAMS.mapBy(x => if (xs.contains(x)) EMPTY_COPY else cl.copies.get(x)),
    need: STREAMS.mapBy(x => not(xs.contains(x)) and cl.need.get(x)),
    more: STREAMS.mapBy(x => not(xs.contains(x)) and cl.more.get(x)),
    seen: STREAMS.mapBy(x => if (xs.contains(x)) 0 else cl.seen.get(x)),
    owed: cl.owed.exclude(xs),
  }

  // A `subscribe` of the client's followed set, replacing an unanswered one.
  pure def sendSubscribe(s: State, c: int): State =
    val so = s.sockets.get(c)
    if (so.open) setSocket(s, c, { ...so, pending: s.clients.get(c).want, hasPending: true }) else s

  /// A `subscribe` replaces the set (empty, or the same set for fresh `heads`, included);
  /// one not yet answered is replaced (subscribes may coalesce, http §8 D15). The client
  /// drops the copies of streams it no longer follows and fetches the new ones.
  pure def resubscribe(s: State, c: int, w: Set[int]): Out =
    val cl = s.clients.get(c)
    val gone = cl.want.exclude(w)
    val cl1 = dropStreams(cl, gone, s.nextId)
    val cl2 = { ...cl1, want: w, need: STREAMS.mapBy(x => cl1.need.get(x) or (w.contains(x) and not(cl.want.contains(x)))) }
    start(sendSubscribe(setClient(s, c, cl2), c))

  pure def issue(s: State, c: int, x: int, dir: Dir, q: Query): Out =
    val id = s.nextId
    val cl = s.clients.get(c)
    val cl2 = match dir {
      | Fwd => {
          ...cl, fwd: cl.fwd.set(x, id), lastFwd: cl.lastFwd.set(x, id), need: cl.need.set(x, false), more: cl.more.set(x, false),
          owed: cl.owed.exclude(Set(x)),
        }
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

  /// A backward page before the oldest event held, while older events exist (mvp §2.4
  /// "Backward history").
  pure def canFetchBack(s: State, c: int, x: int): bool =
    val cl = s.clients.get(c)
    val cp = cl.copies.get(x)
    and { cl.want.contains(x), cl.bwd.get(x) == 0, cp.epoch != 0, cp.moreBefore }
  pure def fetchBack(s: State, c: int, x: int, limit: int): Out =
    val cl = s.clients.get(c)
    issue(s, c, x, Bwd, { after: NONE, before: loOf(cl.copies.get(x)), limit: limit, epoch: cl.epoch })

  /// The client gives up on a request (its connection failed or timed out): it is no
  /// longer current, and a forward fetch is due again.
  pure def timeout(s: State, c: int, x: int, dir: Dir): Out =
    val cl = s.clients.get(c)
    val cl2 = match dir {
      | Fwd => { ...cl, fwd: cl.fwd.set(x, 0), need: cl.need.set(x, cl.want.contains(x)), cancelled: cl.cancelled.union(Set(cl.fwd.get(x))) }
      | Bwd => { ...cl, bwd: cl.bwd.set(x, 0), cancelled: cl.cancelled.union(Set(cl.bwd.get(x))) }
    }
    start(setClient(s, c, cl2))

  /// `GET /api/events`: existence, the head, the decision, and the page or snapshot from
  /// one read snapshot of the current database (mvp §2.4; store §2.7; http §2.7).
  pure def serve(s: State, id: int): Out =
    val r = s.reqs.get(id)
    val h = s.host
    val s1 = { ...s, reqs: without(s.reqs, id) }
    if (not(hasStream(h, r.stream)))
      { st: { ...s1, resps: s.resps.put(id, { req: r, epoch: h.epoch, body: NotFound }) }, calls: [] }
    else {
      val evs = logOf(h, h.epoch, r.stream)
      val hd = evs.length()
      val after = if (r.q.after == NONE) 0 else r.q.after
      val sh = { headSeq: hd, cumHead: cumAt(evs, hd), cumAfter: if (after > hd) 0 else cumAt(evs, after) }
      val plan = decideReplay(r.q, sh, h.epoch)
      val body = match plan {
        | PForward(f) => {
            val n = min2(hd, f.after + f.limit)
            Page({ events: evs.slice(f.after, n), hasMore: hd > n, hasMoreBefore: f.after >= 1 })
          }
        | PBackward(b) => {
            val top = min2(b.before - 1, hd)
            val bottom = max2(1, top - b.limit + 1)
            Page({
              events: if (top >= 1) evs.slice(bottom - 1, top) else [],
              hasMore: hd >= max2(b.before, 1),
              hasMoreBefore: top >= 1 and bottom > 1,
            })
          }
        | PSnapshot(_) => Snap({
            asOf: hd,
            recent: if (r.stream == HOST) [] else evs.slice(max2(0, hd - SNAPSHOT_RECENT_EVENTS), hd),
            proj: projOf(evs),
            threadStatus: if (r.stream == HOST and hasStream(h, THREAD)) projOf(logOf(h, h.epoch, THREAD)).status else NONE,
          })
        | PRefused(_) => BadQuery
      }
      {
        st: { ...s1, resps: s.resps.put(id, { req: r, epoch: h.epoch, body: body }) },
        calls: [DecideReplay({ q: r.q, head: sh, epoch: h.epoch, plan: plan })],
      }
    }

  /// A read that fails before it answers (408 at its cutoff, 503 `busy`).
  pure def failRead(s: State, id: int): Out =
    val r = s.reqs.get(id)
    start({ ...s, reqs: without(s.reqs, id), resps: s.resps.put(id, { req: r, epoch: s.host.epoch, body: ReadFailed }) })

  pure def appendEvs(cp: Copy, e: int, evs: List[Ev]): Copy = {
    ...cp,
    epoch: e,
    events: cp.events.concat(evs),
    hi: if (evs.length() == 0) cp.hi else evs[evs.length() - 1].seq,
    proj: evs.foldl(cp.proj, applyEv),
  }

  /// Establishes or switches the epoch: drops every local copy and stops waiting for every
  /// request issued so far; each followed stream is fetched again.
  pure def switchEpoch(cl: Client, e: int): Client = {
    ...clearRequests(cl, STREAMS),
    epoch: e,
    copies: STREAMS.mapBy(_ => EMPTY_COPY),
    need: STREAMS.mapBy(x => cl.want.contains(x)),
    more: STREAMS.mapBy(falses),
  }

  pure def isCurrent(cl: Client, r: Req): bool = match r.dir {
    | Fwd => cl.fwd.get(r.stream) == r.id
    | Bwd => cl.bwd.get(r.stream) == r.id
  }

  /// The client applies a response (mvp §2.4 "Replay", "Backward history", request
  /// generations): one of an older generation changes nothing; a forward response with
  /// another epoch than the one held (none included) establishes or switches the epoch,
  /// becoming the stream's copy only if it starts at the stream's beginning; a backward
  /// page with another epoch only makes a forward resync due; a 404 ends following the
  /// stream; a failed forward read makes the forward fetch due again, and a failed backward
  /// read only ends that request.
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
      match p.body {
        | NotFound => start(sendSubscribe(setClient(s0, c, dropStreams(cl1, Set(x), s.nextId)), c))
        | _ => {
            val cl2 = match p.body {
              | ReadFailed => match p.req.dir { | Fwd => { ...cl1, need: cl1.need.set(x, true), owed: cl1.owed.union(Set(x)) } | Bwd => cl1 }
              | Page(pg) => match p.req.dir {
                  | Fwd =>
                    if (p.epoch != cl1.epoch) {
                      val sw = switchEpoch(cl1, p.epoch)
                      if (p.req.q.after > 0) sw
                      else {
                        val cp = { ...appendEvs(EMPTY_COPY, p.epoch, pg.events), moreBefore: pg.hasMoreBefore }
                        // A resync asked for after this request was issued stays due.
                        { ...sw, copies: sw.copies.set(x, cp), need: sw.need.set(x, cl1.need.get(x)), more: sw.more.set(x, pg.hasMore) }
                      }
                    } else {
                      val old = cl1.copies.get(x)
                      val cp0 = appendEvs(old, p.epoch, pg.events)
                      val cp = if (old.epoch == 0) { ...cp0, moreBefore: pg.hasMoreBefore } else cp0
                      { ...cl1, copies: cl1.copies.set(x, cp), more: cl1.more.set(x, pg.hasMore) }
                    }
                  | Bwd =>
                    if (p.epoch != cl1.epoch) { ...cl1, need: cl1.need.set(x, true), owed: cl1.owed.union(Set(x)) }
                    else {
                      val cp = cl1.copies.get(x)
                      { ...cl1, copies: cl1.copies.set(x, { ...cp, events: pg.events.concat(cp.events), moreBefore: pg.hasMoreBefore }) }
                    }
                }
              | Snap(sn) => {
                  val cp = {
                    epoch: p.epoch, events: sn.recent, hi: sn.asOf, proj: sn.proj,
                    threadStatus: sn.threadStatus, moreBefore: sn.asOf - sn.recent.length() >= 1,
                  }
                  // A snapshot replaces the stream's copy: the stream's other requests were
                  // built from the old copy and are of an older generation.
                  val base = if (p.epoch != cl1.epoch) switchEpoch(cl1, p.epoch)
                    else clearRequests(cl1, Set(x))
                  // The fetch consumed the stream's `need` when it was issued; a resync asked
                  // for since then stays due.
                  { ...base, copies: base.copies.set(x, cp), need: base.need.set(x, cl1.need.get(x)), more: base.more.set(x, false) }
                }
              | _ => cl1
            }
            // The ghost, at the events themselves: an established or switched epoch makes
            // every earlier request older, a snapshot that replaced a copy that stream's.
            val cl3 = if (cl2.epoch != cl.epoch) markOlder(cl2, STREAMS, s.nextId)
              else match p.body { | Snap(_) => markOlder(cl2, Set(x), s.nextId) | _ => cl2 }
            start(setClient(s0, c, cl3))
          }
      }
    }

  pure def headsOf(s: State, xs: Set[int]): int -> int = xs.mapBy(x => headOf(s.host, x))

  /// The server answers the latest subscribe: `S` becomes its streams that exist, their
  /// heads merge into `known`, and a `heads` frame is due.
  pure def serverSubscribe(s: State, c: int): Out =
    val so = s.sockets.get(c)
    val subs = so.pending.filter(x => hasStream(s.host, x))
    start(setSocket(s, c, {
      ...so,
      subs: subs,
      hasPending: false,
      known: headsOf(s, subs),
      announced: subs.mapBy(x => if (so.announced.keys().contains(x)) so.announced.get(x) else 0),
      headsDue: true,
    }))

  /// A heads refresh (due every 30 s): the heads of `S` merge into `known` and a `heads`
  /// frame is due; refreshes due before it is sent coalesce.
  pure def headsRefresh(s: State, c: int): Out =
    val so = s.sockets.get(c)
    start(setSocket(s, c, { ...so, known: headsOf(s, so.subs), headsDue: true }))

  pure def advancing(so: Socket): Set[int] = so.subs.filter(x => so.known.get(x) > so.announced.get(x))

  pure def canSend(so: Socket): bool = so.open and so.frame == NoFrame and (so.headsDue or advancing(so).size() > 0)

  /// Send selection (http §2.6): `heads` with every stream of `S` if due, else
  /// `stream_advanced` for one stream whose known head is newer than the announced one.
  pure def sendFrame(s: State, c: int): Out =
    val so = s.sockets.get(c)
    val n = if (so.headsDue) Heads(so.known) else {
      val x = minOf(advancing(so))
      Advanced({ stream: x, head: so.known.get(x) })
    }
    val announced = match n {
      | Heads(m) => m
      | Advanced(a) => so.announced.set(a.stream, a.head)
    }
    start(setSocket(s, c, { ...so, frame: InFlight(n), announced: announced, headsDue: false }))

  pure def noteNames(n: Note): Set[int] = match n {
    | Advanced(a) => Set(a.stream)
    | Heads(m) => m.keys()
  }

  /// Notify-then-pull: the client records the newest head it observed per followed stream;
  /// a `heads` frame is a snapshot of every listed stream's head.
  pure def recvNote(s: State, c: int, n: Note): Out =
    val so = s.sockets.get(c)
    val cl = s.clients.get(c)
    val seen2 = STREAMS.mapBy(x => {
      val h = match n {
        | Advanced(a) => if (a.stream == x) a.head else 0
        | Heads(m) => if (m.keys().contains(x)) m.get(x) else 0
      }
      if (cl.want.contains(x)) max2(cl.seen.get(x), h) else cl.seen.get(x)
    })
    start(setSocket(setClient(s, c, { ...cl, seen: seen2 }), c, { ...so, frame: NoFrame }))

  // ---------------------------------------------------------------------------
  // Invariants as functions of the step (each cites its source; the vals below)
  // ---------------------------------------------------------------------------

  pure def copiesOf(s: State): Set[{ c: int, x: int, cp: Copy }] =
    tuples(CLIENTS, STREAMS).map(p => { c: p._1, x: p._2, cp: s.clients.get(p._1).copies.get(p._2) })

  /// mvp §2.4 (CL-13 "without duplicates"; CL-9): the events a copy holds are
  /// consecutive, none twice, and end at its cursor.
  pure def gapOk(s: State): bool = copiesOf(s).forall(r => {
    val evs = r.cp.events
    val n = evs.length()
    and {
      evs.indices().forall(i => i == 0 or evs[i].seq == evs[i - 1].seq + 1),
      n == 0 or evs[n - 1].seq == r.cp.hi,
    }
  })

  /// mvp §2.4 (CL-13 "end with the server's events"): every event a client holds is the
  /// host's event at that position of the epoch the client holds; the cursor is within
  /// that epoch's head; `moreBefore` says whether events below the oldest one held exist.
  pure def heldOk(s: State): bool = copiesOf(s).forall(r => r.cp.epoch == 0 or {
    val evs = logOf(s.host, r.cp.epoch, r.x)
    and {
      r.cp.hi <= evs.length(),
      r.cp.events.indices().forall(i => {
        val e = r.cp.events[i]
        e.seq >= 1 and e.seq <= evs.length() and evs[e.seq - 1] == e
      }),
      r.cp.moreBefore iff loOf(r.cp) > 1,
    }
  })

  /// mvp §2.4 "switching epoch and local state together": a client's local state is of
  /// one epoch, the one it holds.
  pure def mixedOk(s: State): bool = copiesOf(s).forall(r => {
    val held = s.clients.get(r.c).epoch
    and { r.cp.epoch == 0 or r.cp.epoch == held, r.cp.events.indices().forall(i => r.cp.events[i].epoch == held) }
  })

  /// mvp §2.4 "the client renders that stream from the snapshot and the events after
  /// `as_of_seq`": what a copy renders of its own stream is the host's projection at the
  /// copy's cursor.
  pure def renderedOk(s: State): bool = copiesOf(s).forall(r => r.cp.epoch == 0 or
    r.cp.proj == projOf(logOf(s.host, r.cp.epoch, r.x).slice(0, r.cp.hi)))

  // A response is of an older generation when its request was given up, issued before the
  // client last established or switched epoch, reconnected, or replaced or dropped that
  // stream's copy, or when the client issued a later request for the same stream and
  // direction (mvp §2.4 request generations; recorded ghosts).
  pure def older(cl: Client, r: Req): bool =
    val latest = match r.dir { | Fwd => cl.lastFwd.get(r.stream) | Bwd => cl.lastBwd.get(r.stream) }
    or { cl.cancelled.contains(r.id), r.id <= cl.resetAt.get(r.stream), r.id < latest }

  /// mvp §2.4 "a response from an older request generation is discarded".
  pure def olderOk(prev: int -> Client, a: Action, s: State): bool = match a {
    | Deliver(p) => older(prev.get(p.req.client), p.req) implies s.clients.get(p.req.client) == prev.get(p.req.client)
    | _ => true
  }

  /// A response the client applied keeps what the copy held and adds what the response
  /// carried: a forward page keeps the held events and appends its own, a backward page
  /// prepends its own, a snapshot holds exactly its recent events up to `as_of_seq`;
  /// a page moves the cursor to its last event (from the cursor before it and the page).
  /// A response that establishes or switches the epoch starts from an empty copy: a forward
  /// page that does not start at the stream's beginning leaves it empty.
  pure def keptOk(prev: int -> Client, a: Action, s: State): bool = match a {
    | Deliver(p) => {
        val before = prev.get(p.req.client)
        val after = s.clients.get(p.req.client)
        val switched = before.epoch != p.epoch
        val was = if (switched) EMPTY_COPY else before.copies.get(p.req.stream)
        val now = after.copies.get(p.req.stream)
        val applied = not(older(before, p.req)) and p.epoch == after.epoch
        applied implies match p.body {
          | Page(pg) => match p.req.dir {
              | Fwd =>
                if (switched and p.req.q.after > 0) seqsOf(now.events) == Set() and now.hi == 0
                else and {
                  seqsOf(now.events) == seqsOf(was.events).union(seqsOf(pg.events)),
                  now.hi == (if (pg.events.length() == 0) was.hi else pg.events[pg.events.length() - 1].seq),
                }
              | Bwd => seqsOf(now.events) == seqsOf(was.events).union(seqsOf(pg.events)) and now.hi == was.hi
            }
          | Snap(sn) => seqsOf(now.events) == seqsOf(sn.recent) and now.hi == sn.asOf
          | _ => true
        }
      }
    | _ => true
  }

  /// mvp §2.4 "Its first successful forward response (a replay page or a snapshot)
  /// establishes its epoch" and "A failed read …, a backward page, and a response from an
  /// older request generation never set the epoch": a client's epoch changes only on a
  /// successful forward response of the current generation (by the ghosts) that carries
  /// the new epoch.
  pure def establishOk(prev: int -> Client, a: Action, s: State): bool = CLIENTS.forall(c =>
    s.clients.get(c).epoch != prev.get(c).epoch implies match a {
      | Deliver(p) => and {
          p.req.client == c,
          p.req.dir == Fwd,
          match p.body { | Page(_) => true | Snap(_) => true | _ => false },
          not(older(prev.get(c), p.req)),
          s.clients.get(c).epoch == p.epoch,
        }
      // Each step inside a `quiesce` is checked on its own (`last.inner`).
      | Quiesce(_) => true
      | _ => false
    })

  /// mvp §2.4 "Backward history" ("it triggers a forward resync"): a resync asked for after
  /// a stream's forward request was issued is not lost when that request's response is
  /// applied: the stream stays due until a later forward request is issued.
  pure def owedOk(s: State): bool = CLIENTS.forall(c => {
    val cl = s.clients.get(c)
    cl.owed.forall(x => cl.fwd.get(x) != 0 or dueOf(cl, x))
  })

  /// The same, read the other way: a client never goes back to an epoch the host left.
  /// Epochs are random ids in the contract; the model numbers them in creation order.
  pure def regressOk(prev: int -> Client, s: State): bool = CLIENTS.forall(c => s.clients.get(c).epoch >= prev.get(c).epoch)

  pure def checksOf(prev: State, a: Action, s: State): Checks = {
    gap: gapOk(s),
    held: heldOk(s),
    mixed: mixedOk(s),
    rendered: renderedOk(s),
    kept: keptOk(prev.clients, a, s),
    older: olderOk(prev.clients, a, s),
    regress: regressOk(prev.clients, s),
    establish: establishOk(prev.clients, a, s),
    owed: owedOk(s),
  }

  pure def both(x: Checks, y: Checks): Checks = {
    gap: x.gap and y.gap, held: x.held and y.held, mixed: x.mixed and y.mixed,
    rendered: x.rendered and y.rendered, kept: x.kept and y.kept, older: x.older and y.older,
    regress: x.regress and y.regress, establish: x.establish and y.establish,
    owed: x.owed and y.owed,
  }

  // ---------------------------------------------------------------------------
  // Convergence: connectivity returns, writes stop, and the protocol runs on a
  // deterministic fair schedule for a bounded number of steps (bounded liveness); the
  // safety invariants are checked after every delivery inside
  // ---------------------------------------------------------------------------

  /// Some followed stream is behind the host in the same epoch after a lost frame named
  /// it, with no fetch due or in flight, no pending subscribe, and no frame for it to come.
  pure def onlyHeadsRecover(s: State): bool = tuples(CLIENTS, STREAMS).exists(p => {
    val cl = s.clients.get(p._1)
    val so = s.sockets.get(p._1)
    val cp = cl.copies.get(p._2)
    and {
      cl.want.contains(p._2), cl.lost.contains(p._2), cp.epoch == s.host.epoch, cp.hi < headOf(s.host, p._2),
      not(dueOf(cl, p._2)), cl.fwd.get(p._2) == 0, so.open, not(so.hasPending), not(so.headsDue),
      not(advancing(so).contains(p._2)),
      not(frameNames(so).contains(p._2)),
    }
  })

  // One step inside `quiesce`: its result, with its checks folded in. The invariants read
  // the host, the clients' copies and epochs, the step's response, and (`owedOk`) whether
  // an owed stream is due or fetched. Only a delivery changes copies or epochs (the host
  // does not write during a `quiesce`); the other steps only make a stream due or fetched
  // (a fetch pays the owed resync, a timeout or reconnect sets `need`, a frame raises
  // `seen`), so they are not checked. An invariant that reads anything else, or that
  // another step could falsify (`sockets`, a stream stopping being due), must widen the
  // steps checked here.
  type Drain = { o: Out, refreshes: int, done: bool, checks: Checks }

  pure def drainStep(d: Drain, a: Action, o2: Out): Drain = {
    ...d,
    o: { st: o2.st, calls: d.o.calls.concat(o2.calls) },
    checks: match a { | Deliver(_) => both(d.checks, checksOf(d.o.st, a, o2.st)) | _ => d.checks },
  }

  pure def restore(d: Drain): Drain =
    CLIENTS.fold(d, (e, c) => if (e.o.st.sockets.get(c).open) e else drainStep(e, Connect(c), connect(e.o.st, c)))

  pure def dueStreams(s: State, c: int): Set[int] = STREAMS.filter(x => dueOf(s.clients.get(c), x))

  pure def lostStreams(s: State, c: int): Set[int] =
    val cl = s.clients.get(c)
    STREAMS.filter(x => cl.fwd.get(x) != 0 and not(s.reqs.keys().contains(cl.fwd.get(x)) or s.resps.keys().contains(cl.fwd.get(x))))

  /// One step of a fixed schedule: responses, requests, subscribes, frames received,
  /// frames sent, due fetches, timeouts of lost requests, then heads refreshes.
  pure def drainOne(d: Drain): Drain = if (d.done) d else {
    val s = d.o.st
    val subscribing = CLIENTS.filter(c => s.sockets.get(c).open and s.sockets.get(c).hasPending)
    val notified = CLIENTS.filter(c => s.sockets.get(c).open and s.sockets.get(c).frame != NoFrame)
    val sending = CLIENTS.filter(c => canSend(s.sockets.get(c)))
    val fetching = CLIENTS.filter(c => dueStreams(s, c).size() > 0)
    val stuck = CLIENTS.filter(c => lostStreams(s, c).size() > 0)
    if (s.resps.keys().size() > 0) {
      val id = minOf(s.resps.keys())
      drainStep(d, Deliver(s.resps.get(id)), deliver(s, id))
    } else if (s.reqs.keys().size() > 0) {
      val id = minOf(s.reqs.keys())
      drainStep(d, Serve(s.reqs.get(id)), serve(s, id))
    } else if (subscribing.size() > 0) {
      drainStep(d, ServerSubscribe(minOf(subscribing)), serverSubscribe(s, minOf(subscribing)))
    } else if (notified.size() > 0) {
      val c = minOf(notified)
      drainStep(d, RecvNote({ client: c, note: frameNote(s.sockets.get(c)) }), recvNote(s, c, frameNote(s.sockets.get(c))))
    } else if (sending.size() > 0) {
      val c = minOf(sending)
      val o2 = sendFrame(s, c)
      drainStep(d, SendFrame({ client: c, note: frameNote(o2.st.sockets.get(c)) }), o2)
    } else if (fetching.size() > 0) {
      val c = minOf(fetching)
      val o2 = fetch(s, c, minOf(dueStreams(s, c)), 2)
      drainStep(d, Fetch(o2.st.reqs.get(s.nextId)), o2)
    } else if (stuck.size() > 0) {
      val c = minOf(stuck)
      val x = minOf(lostStreams(s, c))
      drainStep(d, Timeout({ client: c, stream: x, dir: Fwd }), timeout(s, c, x, Fwd))
    } else if (d.refreshes > 0) {
      val open = CLIENTS.filter(c => s.sockets.get(c).open)
      val d2 = open.fold(d, (e, c) => drainStep(e, HeadsRefresh(c), headsRefresh(e.o.st, c)))
      { ...d2, refreshes: d.refreshes - 1 }
    } else { ...d, done: true }
  }

  /// Up to DRAIN_STEPS steps of the schedule, from the state after reconnecting.
  pure def drain(restored: Drain): Drain = range(0, DRAIN_STEPS).foldl(restored, (d, _) => drainOne(d))

  /// Every client follows only streams the host has, and has, for each, the host's epoch,
  /// cursor, and rendered state.
  pure def converged(s: State): bool = CLIENTS.forall(c => {
    val cl = s.clients.get(c)
    and {
      cl.want.forall(x => hasStream(s.host, x)),
      cl.want.forall(x => {
        val cp = cl.copies.get(x)
        val evs = logOf(s.host, s.host.epoch, x)
        and { cl.epoch == s.host.epoch, cp.epoch == s.host.epoch, cp.hi == evs.length(), cp.proj == projOf(evs) }
      }),
    }
  })

  // ---------------------------------------------------------------------------
  // Actions
  // ---------------------------------------------------------------------------

  action commitWith(a: Action, o: Out, inner: Checks): bool = all {
    st' = o.st,
    last' = { act: a, calls: o.calls, prevClients: st.clients, inner: inner },
  }

  action commit(a: Action, o: Out): bool = commitWith(a, o, ALL_OK)

  pure def connectedState(s: State): State =
    CLIENTS.fold(s, (t, c) => serverSubscribe(connect(t, c).st, c).st)

  /// A start the random walk rarely reaches by itself: client 1 holds a host snapshot of
  /// epoch 1 (three events, older events below it); its next forward fetch was answered in
  /// epoch 1 with a snapshot; the database was then replaced, and a backward page was
  /// answered in the new epoch. Both responses are in transit, in either order of arrival.
  pure def racingState(s: State): Out =
    val o1 = serve(fetch(s, 1, HOST, 2).st, 1)
    val s2 = deliver(o1.st, 1).st
    val s3 = appendEv(appendEv(s2, HOST, Plain, 3).st, HOST, Plain, 3).st
    val o4 = serve(fetch(s3, 1, HOST, 2).st, 2)
    val s5 = resetDb(o4.st, true)
    val o6 = serve(fetchBack(s5, 1, HOST, 1).st, 3)
    { st: o6.st, calls: o1.calls.concat(o4.calls).concat(o6.calls) }

  action initFrom(hostEvents: int, threadEvents: int, big: bool, want2: Set[int], connected: bool, racing: bool): bool = all {
    val s0 = initialState(hostEvents, threadEvents, big, want2)
    val o = if (racing) racingState(s0) else start(if (connected) connectedState(s0) else s0)
    val s = o.st
    all {
      st' = s,
      last' = {
        act: Init({
          hostEvents: hostEvents, threadEvents: threadEvents, big: big, want2: want2, connected: connected,
          racing: racing, replayMaxEvents: REPLAY_MAX_EVENTS, replayMaxBytes: REPLAY_MAX_BYTES,
        }),
        calls: o.calls,
        prevClients: s.clients,
        inner: ALL_OK,
      },
    },
  }

  /// Both clients disconnected (the named traces start here).
  action initWith(hostEvents: int, threadEvents: int, big: bool, want2: Set[int]): bool =
    initFrom(hostEvents, threadEvents, big, want2, false, false)

  action init = {
    nondet h = 0.to(3).oneOf()
    nondet t = 0.to(3).oneOf()
    nondet big = Bool.oneOf()
    nondet w = Set(Set(HOST), Set(THREAD), STREAMS).oneOf()
    nondet connected = Bool.oneOf()
    // About half the starts are the swap race, with the three host events it needs. The
    // witness counts come from this mix; with fewer racing starts, `witnessOnlyHeadsRecover`
    // is not reached at the CI size.
    nondet racing = Bool.oneOf()
    initFrom(if (racing) 3 else h, t, big, w, connected and not(racing), racing)
  }

  action appendA(x: int, kind: Kind, sz: int): bool = all {
    hasStream(st.host, x),
    headOf(st.host, x) < MAX_EVENTS,
    commit(Append({ stream: x, kind: kind, size: sz }), appendEv(st, x, kind, sz)),
  }

  action restart: bool = commit(Restart, start(restartHost(st)))

  action reset(threadKept: bool): bool = all {
    st.host.epoch < MAX_EPOCHS,
    commit(ResetDb({ threadKept: threadKept }), start(resetDb(st, threadKept))),
  }

  action connectA(c: int): bool = all {
    not(st.sockets.get(c).open),
    commit(Connect(c), connect(st, c)),
  }

  action disconnectA(c: int): bool = all {
    st.sockets.get(c).open,
    commit(Disconnect(c), disconnect(st, c)),
  }

  action resubscribeA(c: int, w: Set[int]): bool =
    commit(Resubscribe({ client: c, want: w, replacedPending: st.sockets.get(c).open and st.sockets.get(c).hasPending }), resubscribe(st, c, w))

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

  action failReadA(id: int): bool = all {
    st.reqs.keys().contains(id),
    commit(FailRead(st.reqs.get(id)), failRead(st, id)),
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
    commit(Deliver(st.resps.get(id)), deliver(st, id)),
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

  action sendFrameA(c: int): bool = all {
    canSend(st.sockets.get(c)),
    val o = sendFrame(st, c)
    commit(SendFrame({ client: c, note: frameNote(o.st.sockets.get(c)) }), o),
  }

  action recvNoteA(c: int): bool = all {
    st.sockets.get(c).open,
    st.sockets.get(c).frame != NoFrame,
    val n = frameNote(st.sockets.get(c))
    commit(RecvNote({ client: c, note: n }), recvNote(st, c, n)),
  }

  /// A lost notification: the frame in flight never reaches the client (its socket's
  /// receiver lagged). The client's ghost records which streams it named.
  action dropNote(c: int): bool = all {
    val so = st.sockets.get(c)
    all {
      so.frame != NoFrame,
      val cl = st.clients.get(c)
      commit(DropNote({ client: c, note: frameNote(so) }), start(
        setClient(setSocket(st, c, { ...so, frame: NoFrame }), c, { ...cl, lost: cl.lost.union(frameNames(so)) }))),
    },
  }

  /// Connectivity returns (every client without a socket reconnects) and writes stop; the
  /// protocol then runs up to DRAIN_STEPS steps of a fair deterministic schedule (`drain`),
  /// with the safety invariants checked after every delivery.
  action quiesce: bool = {
    val restored = restore({ o: start(st), refreshes: DRAIN_REFRESHES, done: false, checks: ALL_OK })
    val d = drain(restored)
    commitWith(Quiesce({ onlyHeads: onlyHeadsRecover(restored.o.st) }), d.o, d.checks)
  }

  action networkStep = any {
    nondet id = orNone(st.reqs.keys()).oneOf()
    any { serveA(id), serveA(id), failReadA(id), loseReq(id) },
    nondet id = orNone(st.resps.keys()).oneOf()
    any { deliverA(id), deliverA(id), loseResp(id) },
    nondet c = CLIENTS.oneOf()
    any { serverSubscribeA(c), sendFrameA(c), sendFrameA(c), recvNoteA(c), recvNoteA(c), dropNote(c), headsRefreshA(c) },
  }

  action clientStep = {
    nondet c = CLIENTS.oneOf()
    nondet x = STREAMS.oneOf()
    nondet limit = Set(1, 2).oneOf()
    any { fetchA(c, x, limit), fetchA(c, x, limit), fetchBackA(c, x, limit) }
  }

  // Writes have three times the weight of a restart or a new database.
  action hostStep = any {
    nondet x = STREAMS.oneOf()
    nondet k = Set(Plain, SurfaceUpdate, StatusChange).oneOf()
    nondet sz = Set(1, 3).oneOf()
    any { appendA(x, k, sz), appendA(x, k, sz), appendA(x, k, sz) },
    restart,
    nondet kept = Bool.oneOf()
    reset(kept),
  }

  action disturbanceStep = {
    nondet c = CLIENTS.oneOf()
    nondet x = STREAMS.oneOf()
    nondet dir = Set(Fwd, Bwd).oneOf()
    nondet w = Set(Set(), Set(HOST), Set(THREAD), STREAMS).oneOf()
    any { connectA(c), connectA(c), disconnectA(c), resubscribeA(c, w), timeoutA(c, x, dir), quiesce }
  }

  // Progress (the network and the client) has twice the weight of the host's writes and
  // of disturbances, `quiesce` among them.
  action step = any { networkStep, networkStep, clientStep, clientStep, hostStep, disturbanceStep }

  // ---------------------------------------------------------------------------
  // Invariants: each holds on the state and on every step inside a `quiesce`
  // ---------------------------------------------------------------------------

  /// mvp §2.4; CL-13 "without duplicates"; CL-9.
  val noGapNoDup = gapOk(st) and last.inner.gap
  /// mvp §2.4; CL-13 "end with the server's events".
  val heldEventsOnHost = heldOk(st) and last.inner.held
  /// mvp §2.4 "switching epoch and local state together".
  val noMixedEpochs = mixedOk(st) and last.inner.mixed
  /// mvp §2.4 "the client renders that stream from the snapshot and the events after `as_of_seq`".
  val renderedStateAtCursor = renderedOk(st) and last.inner.rendered
  /// mvp §2.4 "Replay", "Backward history": an applied response drops nothing held.
  val appliedKeepsHeld = keptOk(last.prevClients, last.act, st) and last.inner.kept
  /// mvp §2.4 "the client discards it, and it changes no local state, the epoch included".
  val olderResponsesChangeNothing = olderOk(last.prevClients, last.act, st) and last.inner.older
  /// mvp §2.4 epoch rule and request generations.
  val epochOnlyFromCurrentForward = establishOk(last.prevClients, last.act, st) and last.inner.establish
  val epochNeverRegresses = regressOk(last.prevClients, st) and last.inner.regress
  val owedResyncStaysDue = owedOk(st) and last.inner.owed

  /// mvp §2.4 "Live notifications", CL-13, http §8 D15: after connectivity returns and
  /// writes stop, within DRAIN_STEPS steps of a fair schedule every client follows only
  /// streams the host has and holds, for each, the host's epoch, cursor, and rendered state,
  /// despite lost responses and frames, disconnects, restarts, and epoch changes before it.
  val convergedAfterQuiesce = match last.act {
    | Quiesce(_) => converged(st)
    | _ => true
  }

  val allInvariants = and {
    noGapNoDup,
    heldEventsOnHost,
    noMixedEpochs,
    renderedStateAtCursor,
    appliedKeepsHeld,
    olderResponsesChangeNothing,
    epochOnlyFromCurrentForward,
    epochNeverRegresses,
    owedResyncStaysDue,
    convergedAfterQuiesce,
  }

  // Witnesses: reachability of the interesting states, built from the recorded response,
  // the client before the step, and the result (`--witnesses`; `simulate` fails when one
  // is never reached).
  pure def planned(cs: List[Call], p: Plan): bool = cs.indices().exists(i => match cs[i] { | DecideReplay(d) => d.plan == p })

  // The response was current and of the epoch the client held, before and after.
  def appliedHere(p: Resp): bool = {
    val before = last.prevClients.get(p.req.client)
    not(older(before, p.req)) and p.epoch == before.epoch and p.epoch == st.clients.get(p.req.client).epoch
  }

  // The response was current, the client holds its epoch, and the stream's cursor moved.
  def advancedHere(p: Resp): bool = and {
    not(older(last.prevClients.get(p.req.client), p.req)),
    st.clients.get(p.req.client).epoch == p.epoch,
    st.clients.get(p.req.client).copies.get(p.req.stream).hi > last.prevClients.get(p.req.client).copies.get(p.req.stream).hi,
  }

  val witnessEpochSwitch = match last.act {
    | Deliver(p) => last.prevClients.get(p.req.client).epoch != 0 and st.clients.get(p.req.client).epoch == p.epoch and
        last.prevClients.get(p.req.client).epoch != p.epoch
    | _ => false
  }
  val witnessConvergedAcrossEpochs = match last.act {
    | Quiesce(_) => CLIENTS.exists(c => last.prevClients.get(c).epoch != 0 and last.prevClients.get(c).epoch < st.host.epoch) and converged(st)
    | _ => false
  }
  val witnessOlderForwardArrived = match last.act {
    | Deliver(p) => p.req.dir == Fwd and older(last.prevClients.get(p.req.client), p.req) and
        match p.body { | Page(pg) => pg.events.length() > 0 | Snap(_) => true | _ => false }
    | _ => false
  }
  val witnessBackwardApplied = match last.act {
    | Deliver(p) => p.req.dir == Bwd and appliedHere(p) and match p.body {
        | Page(pg) => pg.events.length() > 0 and
            st.clients.get(p.req.client).copies.get(p.req.stream).events.length() >
              last.prevClients.get(p.req.client).copies.get(p.req.stream).events.length()
        | _ => false
      }
    | _ => false
  }
  val witnessBackwardEpochMismatch = match last.act {
    | Deliver(p) => {
        val before = last.prevClients.get(p.req.client)
        and {
          p.req.dir == Bwd, not(older(before, p.req)), before.epoch != 0, p.epoch != before.epoch,
          match p.body { | Page(_) => true | _ => false },
          st.clients.get(p.req.client).epoch == before.epoch,
          dueOf(st.clients.get(p.req.client), p.req.stream),
        }
      }
    | _ => false
  }
  // A current forward response of the epoch held is applied while a resync asked for after
  // its request was issued is owed; the stream stays due.
  val witnessResyncKept = match last.act {
    | Deliver(p) => p.req.dir == Fwd and appliedHere(p) and
        last.prevClients.get(p.req.client).owed.contains(p.req.stream) and dueOf(st.clients.get(p.req.client), p.req.stream)
    | _ => false
  }
  val witnessTooManyEvents = planned(last.calls, PSnapshot(TooManyEvents))
  val witnessTooManyBytes = planned(last.calls, PSnapshot(TooManyBytes))
  val witnessEpochChanged = planned(last.calls, PSnapshot(EpochChanged))
  val witnessHasMore = match last.act {
    | Deliver(p) => p.req.dir == Fwd and advancedHere(p) and match p.body {
        | Page(pg) => pg.hasMore and dueOf(st.clients.get(p.req.client), p.req.stream)
        | _ => false
      }
    | _ => false
  }
  val witnessOnlyHeadsRecover = match last.act {
    | Quiesce(q) => q.onlyHeads and converged(st)
    | _ => false
  }
  val witnessResubscribeWhilePending = match last.act {
    | Resubscribe(r) => r.replacedPending
    | _ => false
  }
  val witnessVanishedStream = match last.act {
    | Deliver(p) => p.body == NotFound and not(older(last.prevClients.get(p.req.client), p.req)) and
        not(st.clients.get(p.req.client).want.contains(p.req.stream))
    | _ => false
  }
  val witnessReadFailedRetried = match last.act {
    | Deliver(p) => p.body == ReadFailed and p.req.dir == Fwd and not(older(last.prevClients.get(p.req.client), p.req)) and
        dueOf(st.clients.get(p.req.client), p.req.stream)
    | _ => false
  }
}
