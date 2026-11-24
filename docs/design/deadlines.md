# Design: One Deadline Rule for Async Waits

Status: design. Scope: the daemon's async waits that end at a deadline. The synchronous side already has its rule (`docs/design/daemon-lifecycle.md` §2.10 "Deadlines of start and stop", the four helpers of `pty-runtime::host`). This document gives the async side one module that carries the same classification, and a lint that makes every raw tokio timer in the daemon either go through that module or sit in a small, reviewed, named exception.

This is its own document rather than a section of `http-surface.md` because the rule spans three designs (HTTP, hook ingest, lifecycle) and the lint covers the whole daemon crate.

## 1. Purpose

Reviews of the HTTP, lifecycle, and hook PRs kept finding the same defect in new shapes: a completion racing its deadline. `tokio::time::timeout_at` polls the inner future before its timer, so a result that became ready after the deadline is still delivered; a `select!` against a sleep has the same race unless something samples the clock after the result. Each fix was local: one clock sample after `timeout_at` in `http/ctx.rs` (`3150737`), the per-poll check of `before` in `http/ws.rs` (`6a5f2d6`), `deadline_first` in `lifecycle/shutdown.rs` (`1f6567c`). The daemon now has three hand-written rules and nothing stops a fourth.

After this PR:

- `deadline::settle_before` and `deadline::admit_before` classify an awaited result against a deadline, and their return type makes each caller say whether a late result counts.
- `crates/daemon/clippy.toml` forbids `tokio::time::{timeout, timeout_at, sleep, sleep_until}`. A new timer in the daemon goes through `deadline`, or through a new small function carrying its own `allow` and a reason that names its rule, which review sees in the diff.
- **No observable behaviour changes**: status codes, which result is taken, which futures are polled, and side effects stay as they are. A site moves only where that holds (§3 D3); the others keep their code under a named exception.

## 2. Survey

### 2.1 Regex sites

Command: `rg -n 'timeout_at|time::timeout|sleep_until|select!' crates --type rust`, non-test files only (paths without a `tests/` segment). 117 lines in all, **36 in non-test files**, all listed below. `crates/cli/src/daemon_cmd.rs` and `crates/pty-runtime/src/host/*` have no match: they are synchronous and already decide through the §2.10 helpers.

Kinds: **(a)** a result or state claim whose acceptance is decided against a deadline; **(b)** a plain bound on a wait, where a late result is harmless or discarded; **(c)** other (import, comment, periodic tick, backoff, shutdown signal, std sleep).

| Site | Kind | Current ordering | Correct today | Contract |
|---|---|---|---|---|
| `daemon/src/hook/mod.rs:317`, `:341` | a | Pid wait, a two-arm `biased` `select!`: timer, then the commit receiver. A notification only wakes the loop: `passed(pid_deadline)` is sampled after it, and the strict `read_by` re-read decides. `RecvError::Closed` returns `Ok(false)` (no decision) without that sample | yes; the `Closed` exit makes no claim (the store is closing, and the delivery job then fails 503 `shutting_down`) | hook-ingest §2.3 "Pid wait" |
| `daemon/src/http/ws.rs:27` | c | import of `sleep_until` | n/a | — |
| `daemon/src/http/ws.rs:400` | c | doc comment of `before` | n/a | — |
| `daemon/src/http/ws.rs:404` | a | `before`: timer, then tokio clock check, then poll; no sample after `Ready` | yes | http-surface §2.6 "Bounded disconnect" (clock sampled before each poll of a send) |
| `daemon/src/http/ws.rs:475`, `:480` | a | Startup read, three-arm `biased`: shutdown, timer, join; the join's result classified by `take_read`'s one sample | yes | http-surface §2.3 "one cutoff", §2.6 |
| `daemon/src/http/ws.rs:536`, `:539`, `:540` | a | Socket loop: `biased` send and read timers drop the socket; a read result goes through `take_read`; a send through `before` | yes | http-surface §2.6 |
| `daemon/src/http/ws.rs:541` | b | Idle timer arm, polled before `stream.next()`: a ready idle timer wins a tie. A frame that `stream.next()` returns after the idle instant but before the timer is seen ready renews `idle_at` | yes for the tie; see follow-up F3 | http-surface §2.6, mvp OQ4 timers |
| `daemon/src/http/ws.rs:543` | c | `heads_interval` tick | n/a | http-surface §2.6 |
| `daemon/src/http/ctx.rs:14` | c | import of `timeout_at` | n/a | — |
| `daemon/src/http/ctx.rs:71` | a | `read_cut`: `timeout_at(cutoff)`, then one sample `passed(deadline)` classifies the result; `cutoff` converted before the read is spawned | yes | http-surface §2.3 "one cutoff" (H16) |
| `daemon/src/http/ctx.rs:146` | a | `until_deadline`: `timeout_at(cutoff, start())`, then `passed(deadline)`; `cutoff` converted before `start()` | yes | http-surface §2.3 (read mounts, `body_within`); hook-ingest walk |
| `daemon/src/http/ctx.rs:294` | a | `await_write_releasing`: `timeout_at` to `reply_by`, then `passed(reply_by)` → `outcome_unknown`; `cutoff` converted before the observer registers | yes | http-surface §2.3 plain writes |
| `daemon/src/http/listener.rs:44`, `:52`, `:68`, `:116` | c | Shutdown signal against permit, accept, the 1 s accept backoff, the connection | n/a | http-surface §2.2 |
| `daemon/src/http/commands.rs:31` | c | import of `timeout_at` | n/a | — |
| `daemon/src/http/commands.rs:568` | a | `await_reply`: progress read, then one sample; `timeout_at(cutoff, rx.changed())` only wakes the loop; `cutoff` converted once before the loop | yes | http-surface §2.8 |
| `daemon/src/http/commands.rs:751` | c | Completion backoff; the budget is decided by the clock read at the loop's top | n/a | http-surface §2.8 completion |
| `daemon/src/http/registry.rs:238`, `:240` | b | `wait_empty_until`, a two-arm unbiased `select!` (`notified` or timer, polled in random order); `ServeHandle::shutdown` discards the bool | yes; the report's own semantics: F2 | http-surface §2.2 "Shutdown" |
| `daemon/src/http/upgrade.rs:185` | b | Hand-over: `timeout(hand_over, rx)` polls `rx` first, so a socket queued before the bound is served even when the task resumes after it | yes | http-surface §2.4 hand-over bound |
| `pty-runtime/src/fake.rs:336`, `:348`, `:388`, `:394`, `:400`, `:403` | c | FakePty's own std `sleep_until` (thread sleep) | n/a | pty-runtime FakePty |
| `daemon/src/lifecycle/shutdown.rs:115` | a | `deadline_first`: std clock check, poll, timer; no sample after `Ready` | yes | daemon-lifecycle §2.8 ("a poll admitted before the cutoff can return `Ready`") |
| `daemon/src/lifecycle/shutdown.rs:204` | c | `guarded`: console end against the step's future | n/a | daemon-lifecycle §2.8 |
| `daemon/src/lifecycle/shutdown.rs:513` | a | First wait: `biased` join; a join taken then is refused by `join_ended()`'s sample | yes | daemon-lifecycle §2.8 |
| `daemon/src/lifecycle/shutdown.rs:611` | b | Participants: `timeout_at`, polls the participant first, result discarded | yes | daemon-lifecycle §2.8 (plain `timeout_at`) |

Per line: **(a) 14, (b) 5, (c) 17**.

### 2.2 Decisions outside the regex

| Site | Kind | What decides | Status |
|---|---|---|---|
| `ws::take_read` (startup `ws.rs:481`, loop) | a | one server-clock sample when the result is taken | correct |
| `commands::await_reply` progress read (`commands.rs:559`–`:566`) | a | progress read, then one sample: timely `Done` is the reply, `IntentCommitted` at reply time 409, late `Done` or `Submitted` 503 | correct |
| `shutdown::join_ended` (`shutdown.rs:258`) | a | std sample against `C − startup_join_before` | correct |
| `hook::pid_wait` re-read | a | strict `read_by`: one sample when the wait returns | correct |
| Admission checks: guard (`guard.rs:306`), write (`ctx.rs:284`), command (`commands.rs:524`), ancestry walk (`hook/mod.rs:236`) | action | `passed(deadline)` before the action (§2.10 `act_before` on the async side) | correct; a check and its action are separate steps (the scheduling pause of §2.10) |
| Ticket and pair-code expiry (`tickets.rs:59`–`:141`) | sync | `now < expires_at` under the map's mutex | correct; synchronous, outside this design |
| Store blocking waits (`store/readers.rs:98`, `:171`; `store/mod.rs:349`, `:388`) | sync | their own deadline loops | governed by event-store; outside this design |
| `listener.rs:71` accept backoff (`tokio::time::sleep`) | c | 1 s pause, shutdown wins | n/a |
| `listener.rs:166`, `:172` `ConnIo` write stall (`tokio::time::sleep`) | b | progress first: the stall timer is polled only after the write returned `Pending`, so ready I/O after a pause clears an already-due timer | see F4 |
| `shutdown.rs:234`, `:253` record-removal retry (`tokio::time::sleep`) | b (wait), c (retry) | relative pause computed before its first poll; the deletion is admitted by `Instant::now() >= end` checks | see F1 |
| `http/mod.rs:350` drain report | b | `exited` = closing snapshot minus `abort_requested` at the abort pass | see F2 |

**What was established.** None of the (a) sites above takes a result first observed at or after its deadline as a timely claim. The extended survey found four pre-existing mismatches of other kinds; they are follow-ups (§6), not changed here.

### 2.3 The three hand-written rules

| Helper | Check before poll | Sample after `Ready` | Clock |
|---|---|---|---|
| `ctx` (`timeout_at` + `passed`) | no | yes, one sample classifies | server clock (`MonotonicClock`, injectable) |
| `ws::before` | yes | no | tokio clock |
| `shutdown::deadline_first` | yes | no | std clock |

They agree on outcomes only because each caller's contract tolerates the difference: claims (`ctx`) need the sample after the result; actions (a WebSocket send hands bytes to the transport on every poll) need the check before each poll; lifecycle bounds accept "a poll admitted before the cutoff". Adding the check before the poll to `ctx` would change which result answers (a non-strict reader-pool `ReadError::Busy` polled after the cutoff: 503 `busy` today, 408 after), so the module keeps both orders.

## 3. Decisions

### D1. The helper

```rust
// crates/daemon/src/deadline.rs
pub(crate) enum Settled<T> { InTime(T), Late(T), Elapsed }

impl<T> Settled<T> {
    /// A state claim or a result: counts only in time.
    pub(crate) fn in_time(self) -> Option<T>;
    /// An action or a plain bound: a result of an admitted poll counts.
    pub(crate) fn admitted(self) -> Option<T>;
}

/// Observations: polls `fut` first, as `timeout_at` does.
pub(crate) async fn settle_before<F: Future>(
    now: impl Fn() -> std::time::Instant,
    deadline: std::time::Instant,
    timer: tokio::time::Instant,
    fut: F,
) -> Settled<F::Output>;

/// Actions and admitted bounds: checks the clock before every poll.
pub(crate) async fn admit_before<F: Future>(
    now: impl Fn() -> std::time::Instant,
    deadline: std::time::Instant,
    timer: tokio::time::Instant,
    fut: F,
) -> Settled<F::Output>;
```

**The rule.** Both are `timeout_at(timer, poll_fn(…))` under the module's one `allow`, so tokio's own timer polling (including its cooperative-budget handling) is the one `ctx` uses today. Inside the `poll_fn`, on every wake:

1. `admit_before` only: sample `now()`. At or after `deadline` → `Elapsed`, and `fut` is not polled.
2. Poll `fut`. On `Ready(v)`, sample `now()` once: at or after `deadline` → `Late(v)`, else `InTime(v)`. That one sample classifies the result.
3. On `Pending`, `timeout_at` polls the timer; a fired timer → `Elapsed`.

Step 2 is §2.10's "each return is classified with one clock sample"; step 1 is `act_before`'s check before the action. `settle_before` makes the claim decision of `observed_before` (the sample after) and omits the check before, as `ctx` does today. `in_time()` follows §2.10 for a result **first observed** at or after the deadline, including one produced earlier and left queued.

**Clock contract.** `now` is the clock that decides: the server's `MonotonicClock::now` for HTTP claims, `tokio::time::Instant::now().into_std()` for the WebSocket timers, `std::time::Instant::now` for the coordinator, as each site reads today. `timer` is a tokio instant the caller computes where its code computes it today (`Shared::timer(deadline)` before the read is spawned, before `start()`, before the observer registers, once before the reply loop), because `MonotonicClock::real` depends on the clock's state and a later conversion could arm a different timer than the store's own cutoff. The helper never converts: it holds no `MonotonicClock`, so no adapter exists whose `real` is not a real instant, and nothing can put tokio's paused coordinate into `HttpConfig.clock` or a store wait. Unit tests pass a closure over a test cell, and a far-future `timer` when the timer must not fire.

**Ownership.** On `Elapsed` the helper drops `fut`: a `JoinHandle` detaches its task (a cut-off read finishes on its own, §2.3; a store shutdown thread runs until the process exits, §2.8); a dropped reply receiver discards the reply while the registered observer still sees its job through, runs `after_ok`, and reports a fatal outcome (§2.2); a dropped send future drops its sink. On `Late(v)` the caller owns `v` and drops it (a read result, a blob permit inside it, a reply) or takes it (`admitted()`). The command runner and the whole HTTP drain stay outside the helpers.

**Factories.** `until_deadline(start)` calls `start()` before `settle_before`, as `timeout_at(cutoff, start())` does today. Constructing the handler's future happens after the guard's admission sample, inside the scheduling pause §2.10 already states; moving the construction into an admitted poll would change which futures are polled and is follow-up F5.

**Alternatives.**

| Option | Gains | Gives up | Reversible | Cost |
|---|---|---|---|---|
| **Chosen:** two entry points sharing step 2, mapping per site | one classification; both existing orders kept, so no behaviour change; the late case is a typed value | a site can pick the wrong entry point or mapping; review and §3 D4's tests catch it | yes | one module |
| One function with the check before every poll | one entry point | `read_cut`'s non-strict `Busy` turns 503 → 408; `ctx` futures stop being polled after the cutoff | yes | contract edit in http-surface §2.3 |
| One function without the check | one entry point | a WebSocket send would be polled after its deadline, against §2.6 | yes | contract edit |
| Hand-written `poll_fn` with its own `Sleep` | no `timeout_at` inside | differs from tokio's `Timeout` under an exhausted cooperative budget | yes | same |
| Keep the three local rules, add only the lint | smallest diff | three exceptions and no single rule to point to | yes | low now, recurring later |
| Shared crate | usable outside the daemon | `crates/core` is Sans-IO; `pty-runtime` and the CLI have no tokio in production | yes (moving is mechanical) | new workspace member |

**Home.** `crates/daemon/src/deadline.rs`, daemon-local: the daemon is the only crate with async production code.

### D2. The lint

`crates/daemon/clippy.toml` gains:

```toml
{ path = "tokio::time::timeout", reason = "decide through deadline::{settle_before, admit_before}" },
{ path = "tokio::time::timeout_at", reason = "decide through deadline::{settle_before, admit_before}" },
{ path = "tokio::time::sleep_until", reason = "a wait to a deadline: deadline::*, or a small allowed fn naming its rule" },
{ path = "tokio::time::sleep", reason = "a relative wait can express a deadline: deadline::*, or a small allowed fn naming its rule" },
```

- **Where.** In the daemon's own `clippy.toml`, not a workspace-root one: clippy reads the nearest `clippy.toml` above the crate's manifest and does not merge it with a parent's, so a root file is shadowed by the existing `crates/{core,cli,daemon}/clippy.toml`. Verified with a scratch crate (deleted): a parent's list applied to a child without its own file, and was ignored once the child had one.
- **Coverage.** The scratch crate showed the lint firing on a `use`-imported bare call, the full path, a module alias, a crate re-export, the function taken as a value (as in `deadline.map(sleep_until)`), inside `#[cfg(test)]` modules, and in integration tests under `--all-targets`; an `allow` with a reason on a function suppressed it. `Sleep::reset`, `interval`, `interval_at`, and timers inside libraries (hyper's `header_read_timeout`) stay outside the lint; review owns them.
- **Exception scope.** `clippy::disallowed_methods` also carries the router and spawn-lock entries, so no `allow` covers a module, a file, a `select!`, or a function that does more than build one timer. Each exception is a private function whose body is the timer construction alone, with `#[allow(clippy::disallowed_methods, reason = "…")]` naming the rule its callers follow (the repository's form: `routes::build_router`, `lifecycle::spawn_locked`).
- **Production exceptions (10).**

  | Function | Wraps | Callers' rule (reason) |
  |---|---|---|
  | `deadline::bound` | `timeout_at` | the module's implementation (D1) |
  | `hook::pid_timer` | `sleep_until` | biased timer, then one sample and a strict re-read (hook-ingest §2.3) |
  | `ws::socket_timer` | `sleep_until` | a fired timer drops the socket or ticks; results go through `take_read` or `admit_before` (§2.6) |
  | `commands::reply_wake` | `timeout_at` | wakes the reply loop; the progress read and its sample decide (§2.8) |
  | `commands::backoff_until` | `sleep_until` | backoff; the budget is read before each attempt (§2.8) |
  | `upgrade::hand_over_bound` | `timeout` | plain bound polled socket first (§2.4) |
  | `shutdown::participant_bound` | `timeout_at` | plain bound polled participant first, result discarded (§2.8) |
  | `shutdown::record_pause` | `sleep` | retry pause; deletion admitted by the budget checks (§2.8 step 5) |
  | `listener::accept_backoff` | `sleep` | accept-error backoff, shutdown wins (§2.2) |
  | `listener::stall_timer` | `sleep` | write stall, progress first (§2.2 `ConnIo`) |

  A new caller of one of these functions adds no `allow`; the functions are private to their modules, and their names and reasons say what a caller may use them for.
- **Tests** bound and pause through one module, `crates/daemon/tests/support/waits.rs` (`pub mod waits;` in `tests/support/mod.rs`). That support tree is compiled into the integration tests as `support::waits` and into the unit tests as `crate::store::tests::support::waits` (through the existing `#[path]`), so one source carries the tests' single `allow`:

  ```rust
  /// The one `allow`: test bounds and pauses only; a deadline claim under test is checked
  /// with paused time, an injected clock, or the fault gates, never with this as its oracle.
  pub async fn bounded_at<F: Future>(at: tokio::time::Instant, fut: F) -> Result<F::Output, tokio::time::error::Elapsed>;
  pub async fn bounded<F: Future>(within: Duration, fut: F) -> Result<F::Output, Elapsed>; // bounded_at(now + within)
  pub async fn pause_until(at: tokio::time::Instant); // bounded_at(at, pending())
  pub async fn pause(d: Duration);                    // pause_until(now + d)
  ```

  Each matches the tokio call it replaces (the same timer entry, so paused-time auto-advance is unchanged). The migration is mechanical: `tokio::time::timeout(` → `waits::bounded(`, `timeout_at(` → `waits::bounded_at(`, `sleep_until(` → `waits::pause_until(`, `sleep(` → `waits::pause(`, plus one `use` per file. Count (`rg -o 'tokio::time::(timeout_at|timeout|sleep_until|sleep)\b'` under `tests/` paths): **74 call sites in 22 files** (47 `timeout`, 3 `timeout_at`, 16 `sleep_until`, 8 `sleep`): 40 in 10 unit-test files (`src/hook/tests/{handler,intake}.rs`, `src/http/tests/{commands,faults,layers,reads,registry,upgrade,writes,ws}.rs`) and 34 in 12 integration files (`hook_b8`, `hook_ingest`, `http_caps`, `http_commands`, `http_contract`, `http_events`, `http_layers`, `http_matrix`, `http_server`, `http_ws`, `support/commands.rs`, `support/http.rs`). The 50 `timeout`/`timeout_at` sites are the bounds the earlier estimate counted. No other crate's tests use these functions.
- **The check that the ban fires** (D4).

### D3. Migration scope

**Migrated**, each identical in status codes, the result taken, the futures polled, and side effects:

| Site | Becomes | Why it is identical |
|---|---|---|
| `ctx::read_cut` (`:71`) | `settle_before(server now, deadline, cutoff, handle)` → `read_outcome(settled, strict)` | today's `timeout_at(cutoff)` + one `passed` sample with the sample moved inside; same `cutoff` instant, same polls, same classification; the fault points keep their order (`before_cutoff_wait` before, `after_cutoff_sample` after the sample, before `classify`) |
| `ctx::until_deadline` (`:146`) | `settle_before(…, start())` → `deadline_outcome` | `InTime` is today's `Ok(v) if !passed`; `Late` and `Elapsed` are today's 408 |
| `ctx::await_write_releasing` (`:294`) | `settle_before(…, rx)` → `reply_outcome` | `InTime(Ok(r))` is today's `Ok(Ok(r))` before `reply_by`; everything else is today's 503 `outcome_unknown` |
| `ws::before` callers: close's two sends (`:373`, `:385`), the loop's send (`:545`) | `before(now, …)` = `admit_before(tokio now, …).admitted()` | today's `before` returns `None` iff its timer is ready or the tokio clock passed the deadline before the poll; a ready timer implies the clock passed, so the condition is the clock check of step 1; `Late` maps back through `admitted()` |
| close's peer-close drain (`:388`) | same `before`, result discarded | as above; nothing reads the result |
| `shutdown::deadline_first` callers: stopping rewrite (`:415`), startup join (`:552`), store report (`:650`) | `deadline_first(now, …)` = `admit_before(std now, end, from_std(end), fut)`, `InTime`/`Late` → `Ok`, `Elapsed` → `Err(Elapsed)` | the same order (clock, poll, timer) and the same timer instant; the added sample only feeds `Late`, mapped back to `Ok` |
| `registry::wait_empty_until` (`:240`) | `settle_before(std now, deadline, from_std(deadline), notified())`; `InTime`/`Late` → loop, `Elapsed` → `is_empty()` | the unbiased `select!` polls `notified` or the timer first at random; `timeout_at` polls `notified` first, one of those two orders; the result is `()` |

`ws::before` and `shutdown::deadline_first` stay as thin wrappers so their callers and existing tests are untouched; their bodies hold no deadline logic, and they take `now` so §3 D4 can test their mapping. `hook/mod.rs:243`, `:262` follow through `until_deadline`. The `ctx.rs` comments that say the cutoff is set before the read, `start()`, or the job stay true: the caller still computes `cutoff` there.

**Kept** on their current code, the timer behind its D2 exception:

| Site | Why kept | What `settle_before`/`admit_before` would change |
|---|---|---|
| `hook::pid_wait` (`:317`) | two-arm but `biased` on the timer's readiness | poll-first takes a ready notification over a fired timer, and `RecvError::Closed` then returns `Ok(false)` where today the timer wins; `admit_before` would decide by the server clock, which a skewed test clock separates from the timer |
| WebSocket startup and loop `select!`s (`:475`, `:536`) | three and ten arms | (restructuring, out of scope) |
| `commands::await_reply` wait (`:568`) | the wait only wakes the loop | an extra clock reading per wake whose value is unused; `HeldClock` tests hand a held reading to the next reader, and the extra reading would take it in place of the loop's sample |
| `commands::complete` backoff (`:751`) | backoff | — |
| `upgrade` hand-over (`:185`) | behaviour-changing migration deferred | under `admit_before`: a socket queued before the bound but first polled after it is served (`hello` sent) today, and would be dropped after `101` |
| `shutdown` participants (`:611`) | behaviour-changing migration deferred | under `admit_before`: a participant first polled after `C − store_before` is polled once today, and would not be polled |
| record-removal pause, accept backoff, write stall | pauses and a progress-first stall | — |

The read path's non-strict `Busy` is not a kept site: `settle_before` keeps today's order, so a late `ReadError::Busy` still answers 503 `busy`.

**Documents.** daemon-lifecycle §2.8 already states the coordinator's split (deadline first for the rewrite, the join, and the store report; HTTP's own deadline; a plain `timeout_at` for the participants). The implementing PR names `admit_before` there and `settle_before`/`admit_before` in http-surface §2.3 and §2.6 where they name `timeout_at` or the per-poll sample.

### D4. Test strategy

All deterministic: a counting test clock (a closure over a cell), futures built with `poll_fn` that count their polls, paused tokio time for the timer; no real-time sleeps.

**The helpers** (unit tests in `deadline.rs`):

| Claim | Test | Mutation it kills |
|---|---|---|
| ready before the deadline → `InTime`, one sample | clock one tick before the deadline; ready on first poll; the clock counts exactly one reading (`settle_before`) or two (`admit_before`) | an extra or a missing sample |
| ready during a poll ending at the deadline → `Late(v)` | the future's poll sets the clock to exactly the deadline, then returns `Ready` | step 2's sample removed; `>=` → `>` |
| `admit_before` at the deadline → `Elapsed`, unpolled | clock exactly at the deadline before the first poll; poll count 0 | step 1 removed; step 1's `>=` → `>` |
| `settle_before` ready but first polled after the deadline → `Late(v)`, polled once | clock past the deadline before the first poll | a check before the poll added to `settle_before` |
| every wake is checked | `admit_before`, pending future: woken at the same instant → stays pending, polled again; clock set to the deadline and woken → `Elapsed`, poll count unchanged | step 1 run only on the first poll |
| the timer ends a pending wait | paused time; the waiter in its own spawned task, the watchdog (`waits::bounded`) in the test task; the logical clock held before the deadline; `timer` = tokio now + 10 s, not the logical deadline; never-ready future → `Elapsed`, with `tokio::time::Instant::now() == timer` recorded when the helper returns | timer not polled (the waiter hangs; a watchdog in the same task would re-poll it); timer armed early or from the logical deadline |
| `in_time` / `admitted` | each variant mapped | the two swapped |

**Claim sites: mappings from a supplied `Settled`.** Each migrated claim site maps through a small pure function, unit-tested from constructed values, because the real `Late` path for a reader-pool result needs a real-time race (the pool's wait and the timer end at the same real instant):

- `read_outcome(settled, strict)`: `InTime(Ok(Ok(v)))` → `v`; `Late(Ok(Ok(v)))` → 408; `Late(Ok(Err(Store(DiskFull))))` → 408; `Late(Ok(Err(ReadError::Busy)))` → 503 `busy` without `strict`, 408 with it; `InTime(Ok(Err(ReadError::Busy)))` → 503 both ways; `Late(Err(join))` → 408; `InTime(Err(join))` → 500; `Elapsed` → 408. Kills `Late` → `InTime`, a dropped `strict`, and `Busy` precedence changes.
- `deadline_outcome`: `InTime(v)` → `v`; `Late(v)` and `Elapsed` → 408.
- `reply_outcome`: `InTime(Ok(Ok(r)))` → `r`; `InTime(Ok(Err(f)))` → its `write_failure` mapping; `InTime(Err(_))`, `Late(_)`, `Elapsed` → 503 `outcome_unknown`.
- End to end, `until_deadline` with a supplied future that skews the `SkewClock` past the deadline before returning `Ready` → 408 (`http::tests::reads::until_deadline_late_result_is_408`); kills `in_time()` → `admitted()` at the site.
- Existing tests stay unchanged and must pass: `http::tests::reads::{late_read_success_is_408, late_read_error_is_408, late_body_is_408, late_admission_read_keeps_the_ticket, read_by_cutoff_precedes_the_outcome, read_by_classifies_with_one_sample}` (the last proves a result taken in time stays accepted after a pause), `http::tests::writes::body_after_the_cutoff_submits_nothing`, and the `http::tests::commands` reply-time tests, including `progress_taken_after_the_grace_is_outcome_unknown`.

**Admitted sites.** `ws::before` and `shutdown::deadline_first` take `now`, so each gets two unit tests with a test clock: a future whose poll moves the clock past the deadline and returns `Ready` → `Some(v)` / `Ok(v)` (kills `admitted()` → `in_time()`, which §2.6 and §2.8 forbid: a result of an admitted poll counts), and a clock past the deadline before the first poll → `None` / `Err(Elapsed)` with poll count 0. Existing tests stay: `http::tests::ws::{loop_drops_a_send_ready_after_its_deadline, close_drops_a_send_ready_after_its_deadline, send_deadline_drops_without_a_close_frame, close_keeps_the_send_deadline}`, `lifecycle::tests::helpers::{join_ready_after_its_end_is_a_bare_exit, store_report_after_its_end_keeps_the_lock, unjoined_startup_is_a_bare_exit}`, `http::tests::registry` tests.

**Kept sites** keep their tests: `hook::tests::handler::pid_wait_timings`, `http::tests::upgrade::{hand_over_bound_starts_after_the_ticket_is_removed, shutdown_aborts_a_held_socket}`, `lifecycle::tests::helpers::console_end_after_http_starts_no_participant`.

**The lint.** CI runs `cargo clippy --workspace --all-targets --locked -- -D warnings` on macOS and Windows, so a raw call without an exception fails it. A membership check alone would miss a lint that stops firing (a renamed path, a shadowing `clippy.toml`), so `crates/cli/tests/lints.rs` gains a test that runs clippy on a probe:

- `crates/daemon/lint-probe/`: a crate with its own empty `[workspace]` table (outside the workspace's `crates/*` members), depending only on `tokio` with `time`, pinned by its own `Cargo.lock`, whose `src/lib.rs` calls each of the four functions once. It has no `clippy.toml`, so clippy reads `crates/daemon/clippy.toml`, the file under test.
- The test runs `$CARGO clippy --locked --manifest-path crates/daemon/lint-probe/Cargo.toml --target-dir <workspace target>/lint-probe -- -D clippy::disallowed_methods` through `chaldea_daemon::lifecycle::spawn_locked` (the CLI's `clippy.toml` forbids `Command::output`), and asserts a failing exit and one "use of a disallowed method `tokio::time::<name>`" line per function.
- The existing membership parse also asserts the four paths, so a deleted entry fails with a precise message.

## 4. Purpose Layer and Plan Layer

### 4.1 Purpose Layer: Acceptance Criteria

| ID | Criterion (EARS) | Source | How checked |
|---|---|---|---|
| DL-P-1 | When a read, a guarded handler, or a plain write reply is first observed at or after its deadline, the daemon shall answer 408 (a read or handler) or 503 `outcome_unknown` (a write reply), except that a non-strict read's reader-pool exhaustion answers 503 `busy`; when a pid-wait notification arrives at or after `pid_deadline`, the pid wait shall end as its bound (408 or `unbound`). | http-surface §2.3; hook-ingest §2.3; daemon-lifecycle §2.10 "Late results" | DL-CL-4 to DL-CL-7; `pid_wait_timings` |
| DL-P-2 | If a change adds a call to `tokio::time::timeout`, `timeout_at`, `sleep`, or `sleep_until` in any target of `crates/daemon` outside a function that carries its own `allow`, then CI shall fail. | §1 | CI clippy job; DL-CL-10 |
| DL-P-3 | The daemon's HTTP status codes, reply bodies, WebSocket hand-over, close, and drop behaviour, ticket consumption, cancellation of cut-off work, and shutdown steps shall stay as their designs state. | http-surface, daemon-lifecycle, hook-ingest | the full suite passes with no test edited except the mechanical `waits` replacement and added tests |

### 4.2 Plan Layer: Claims

| ID | Claim (EARS) | Source | Falsifying test |
|---|---|---|---|
| DL-CL-1 | When `admit_before` is woken at or after its deadline, it shall return `Elapsed` without polling the future. | D1 step 1 | unit `deadline::tests::admit_at_the_deadline_is_elapsed_unpolled`, `…::admit_checks_every_wake` |
| DL-CL-2 | When the future returns `Ready`, both helpers shall classify it by one clock sample taken after the poll: `Late(v)` at or after the deadline, `InTime(v)` before it. | D1 step 2; daemon-lifecycle §2.10 | unit `deadline::tests::ready_in_time_takes_one_sample`, `…::ready_at_the_deadline_is_late`, `…::settle_polls_first_after_the_deadline` |
| DL-CL-3 | While the future is pending, both helpers shall return `Elapsed` when the caller's `timer` instant passes, whatever the deciding clock reads. | D1 step 3 | unit `deadline::tests::timer_ends_a_pending_wait` (paused time, waiter in its own task) |
| DL-CL-4 | When a read's result is `Late`, the read shall answer 408, except reader-pool exhaustion on a non-strict read, which answers 503 `busy`. | http-surface §2.3 | unit `http::ctx::tests::read_outcome_table` |
| DL-CL-5 | When a guarded handler's or body's future completes `Late`, `until_deadline` shall answer 408. | http-surface §2.3 | unit `http::ctx::tests::deadline_outcome_table`; `http::tests::reads::until_deadline_late_result_is_408` |
| DL-CL-6 | When a plain write's reply is `Late` or `Elapsed`, `await_write` shall answer 503 `outcome_unknown`. | http-surface §2.3 | unit `http::ctx::tests::reply_outcome_table` |
| DL-CL-7 | When a pid-wait notification wakes the loop at or after `pid_deadline`, the pid wait shall end as its bound. | hook-ingest §2.3 | `hook::tests::handler::pid_wait_timings` (kept site, unchanged) |
| DL-CL-8 | When a WebSocket send or a coordinator wait completes during a poll admitted before its deadline, the site shall take the result; when it is first polled at or after the deadline, the site shall not poll it. | http-surface §2.4, §2.6; daemon-lifecycle §2.8 | unit `http::ws::tests::before_takes_an_admitted_result`, `…::before_skips_a_late_poll`, `lifecycle::shutdown::tests::deadline_first_takes_an_admitted_result`, `…::deadline_first_skips_a_late_poll`; the existing ws and lifecycle tests of D4 |
| DL-CL-9 | The kept sites (hand-over, participants, pid wait, reply wake) shall poll as today. | D3 | `http::tests::upgrade::hand_over_bound_starts_after_the_ticket_is_removed`, `console_end_after_http_starts_no_participant`, `pid_wait_timings`, the `http::tests::commands` reply-time tests |
| DL-CL-10 | Clippy shall reject a call to each of `tokio::time::{timeout, timeout_at, sleep, sleep_until}` in a crate under `crates/daemon` that has no `clippy.toml` of its own. | D2 | `cli/tests/lints.rs::daemon_ban_fires_on_the_probe`; `…::clippy_tomls_list_the_bans` |

## 5. Out of Scope

- The synchronous helpers of `pty-runtime::host`, the CLI's `poll_until`, ticket and pair-code expiry, and the store's blocking waits.
- hyper's `header_read_timeout`, `Sleep::reset`, `interval` (review-only).
- Restructuring the WebSocket `select!`s around the helpers.
- The CLI crate's `clippy.toml`: the CLI has no tokio outside its tests.

## 6. Follow-ups and Open Questions

Pre-existing mismatches, not changed here (a fix changes timing or reporting):

- **F1.** Record removal: the retry pause is relative and computed before its first poll, so a pause between the two can carry the wait past the 500 ms budget, against daemon-lifecycle §2.8 step 5 ("the wait before a retry never runs past the budget"). No deletion follows the budget (it is checked before each deletion). Fix: an absolute wake instant.
- **F2.** HTTP drain report: `exited` is the closing snapshot minus the tasks still registered at the abort pass, so a task that exits between the deadline and `abort_remaining()` counts as `exited`, while `HttpShutdownReport::exited`'s rustdoc says "ended before the deadline". Reconcile the rustdoc with http-surface §2.2 step 4, which counts at the abort pass.
- **F3.** WebSocket idle: renewal follows the idle timer's readiness, not a strict cutoff (§2.1). http-surface §2.6 says a socket "with no inbound frame for `idle_timeout`" is dropped. Decide whether a frame read after the idle instant but before the timer fires may renew.
- **F4.** `ConnIo` write stall is progress first: ready I/O after a pause clears a due stall timer. Decide whether expiry must win.
- **F5.** `until_deadline` builds the handler's future before its first admitted poll; moving construction into the poll would make the guard's admission and the construction one step.

Open questions:

1. `upgrade` hand-over and the participants are kept per this PR's scope, but `settle_before` (poll first, like `timeout` / `timeout_at`) with `admitted()` would migrate both without a behaviour change: `timeout(d, f)` is `timeout_at(now + d, f)`. Move them in this PR, or keep the exceptions?
2. The lint probe compiles tokio once per cache-cold CI run (its own target directory under the workspace target). Acceptable in the test job, or run it as a separate CI step?
