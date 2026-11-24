# Design: One Deadline Rule for Async Waits

Status: design. Scope: the daemon's async waits that end at a deadline. The synchronous side already has its rule (`docs/design/daemon-lifecycle.md` §2.10 "Deadlines of start and stop", the four helpers of `pty-runtime::host`). This document gives the async side one helper for the claim shape the HTTP reads already use, and a lint that puts every raw tokio timer in the daemon behind that helper or behind a small, named, reviewed exception.

This is its own document rather than a section of `http-surface.md` because the rule spans three designs (HTTP, hook ingest, lifecycle) and the lint covers the whole daemon crate.

## 1. Purpose

Reviews of the HTTP, lifecycle, and hook PRs kept finding the same defect in new shapes: a completion racing its deadline. `tokio::time::timeout_at` polls the inner future before its timer, so a result that became ready after the deadline is still delivered; a `select!` against a sleep has the same race unless something samples the clock after the result. Each fix was local: one clock sample after `timeout_at` in `http/ctx.rs` (`3150737`), the per-poll check of `before` in `http/ws.rs` (`6a5f2d6`), `deadline_first` in `lifecycle/shutdown.rs` (`1f6567c`). The daemon now has three hand-written rules and nothing stops a fourth.

After this PR:

- `deadline::settle_before` is the shape `ctx` uses today, made a function: await an owned `timeout_at`, then classify a ready result with one clock sample. Its return type makes each caller say whether a late result counts.
- `crates/daemon/clippy.toml` forbids `tokio::time::{timeout, timeout_at, sleep, sleep_until}`. **The guarantee**: no direct call of a banned function exists outside the named exception functions, the helper, and the test wrappers. A new deadline decision either uses `settle_before` or adds a reviewed exception. A new caller of an existing exception function adds no `allow`; review owns those callers.
- **No observable behaviour changes.** Only the two `ctx` sites already in the helper's shape move to it; every other site keeps its code, with its timer construction moved into a private exception function.

## 2. Survey

### 2.1 Regex sites

Command: `rg -n 'timeout_at|time::timeout|sleep_until|select!' crates --type rust`, non-test files only (paths without a `tests/` segment). 117 lines in all, **36 in non-test files**, all listed below. `crates/cli/src/daemon_cmd.rs` and `crates/pty-runtime/src/host/*` have no match: they are synchronous and already decide through the §2.10 helpers.

Kinds: **(a)** a result or state claim whose acceptance is decided against a deadline; **(b)** a plain bound on a wait, where a late result is harmless or discarded; **(c)** other (import, comment, periodic tick, backoff, shutdown signal, std sleep).

| Site | Kind | Current ordering | Correct today | Contract |
|---|---|---|---|---|
| `daemon/src/hook/mod.rs:317`, `:341` | a | Pid wait, a two-arm `biased` `select!`: timer, then the commit receiver. A matching commit or a lag wakes the loop: `passed(pid_deadline)` is sampled after it, and the strict `read_by` re-read decides; an unrelated commit is skipped. `RecvError::Closed` returns `Ok(false)` (no decision) without that sample | yes; the `Closed` exit makes no claim (the store is closing, and the delivery job then fails 503 `shutting_down`) | hook-ingest §2.3 "Pid wait" |
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
| `daemon/src/http/ctx.rs:294` | a | `await_write_releasing`: `timeout_at` to `reply_by`; a closed channel is 503 `outcome_unknown` with no clock reading; a reply is classified by `passed(reply_by)`; `cutoff` converted before the observer registers | yes | http-surface §2.3 plain writes |
| `daemon/src/http/listener.rs:44`, `:52`, `:68`, `:116` | c | Shutdown signal against permit, accept, the 1 s accept backoff, the connection | n/a | http-surface §2.2 |
| `daemon/src/http/commands.rs:31` | c | import of `timeout_at` | n/a | — |
| `daemon/src/http/commands.rs:568` | a | `await_reply`: progress read, then one sample; `timeout_at(cutoff, rx.changed())` only wakes the loop; `cutoff` converted once before the loop | yes | http-surface §2.8 |
| `daemon/src/http/commands.rs:751` | c | Completion backoff; the budget is decided by the clock read at the loop's top | n/a | http-surface §2.8 completion |
| `daemon/src/http/registry.rs:238`, `:240` | b | `wait_empty_until`, a two-arm unbiased `select!` (`notified` or timer, polled in random order); `ServeHandle::shutdown` discards the bool | yes; the report's rustdoc: F2 | http-surface §2.2 "Shutdown" |
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

They agree on outcomes only because each caller's contract tolerates the difference: claims (`ctx`) need the sample after the result; actions (a WebSocket send hands bytes to the transport on every poll) need the check before each poll; lifecycle bounds accept "a poll admitted before the cutoff". Rewriting the second and third shapes onto a shared helper would move a clock sample, a poll, or a drop relative to the others, so this PR leaves them as they are.

## 3. Decisions

### D1. The helper

```rust
// crates/daemon/src/deadline.rs
pub(crate) enum Settled<T> { InTime(T), Late(T), Elapsed }

impl<T> Settled<T> {
    /// A state claim or a result: counts only in time.
    pub(crate) fn in_time(self) -> Option<T>;
}

pub(crate) async fn settle_before<F: Future>(
    now: impl Fn() -> std::time::Instant,
    deadline: std::time::Instant,
    timer: tokio::time::Instant,
    fut: F,
) -> Settled<F::Output>;
```

**The rule**, in the body's order:

1. `let r = bound(timer, fut).await;` where `bound` is the module's exception function around `tokio::time::timeout_at`. The `Timeout` owns `fut`, so `fut` is dropped when the `.await` completes, before anything below runs.
2. `Err(_)` → `Elapsed`, with no clock reading.
3. `Ok(v)` → one sample `now()`: at or after `deadline` → `Late(v)`, else `InTime(v)`.

This is `ctx::until_deadline` and `ctx::read_cut` today, statement for statement. Step 3 is §2.10's "each return is classified with one clock sample", and `in_time()` follows §2.10 for a result first observed at or after the deadline, including one produced earlier and left queued. Sampling after the drop matters: a future whose destructor runs past the deadline is classified by the later time, as today.

**Clock and timer.** `now` is the clock that decides (the server's `MonotonicClock::now` at both migrated sites). `timer` is a tokio instant the caller computes where its code computes it today (`Shared::timer(deadline)` before the read is spawned, before `start()`), because `MonotonicClock::real` depends on the clock's state. The helper never converts and holds no `MonotonicClock`. Unit tests pass a closure over a test cell, and a far-future `timer` when the timer must not fire.

**Ownership.** On `Elapsed` the `Timeout` has dropped `fut`: a `JoinHandle` detaches its task (a cut-off read finishes on its own, §2.3). On `Late(v)` the caller owns `v` and drops it (a read result, a blob permit inside it).

**Alternatives.**

| Option | Gains | Gives up | Reversible | Cost |
|---|---|---|---|---|
| **Chosen:** one helper in the `ctx` shape; other shapes stay behind exceptions | identical by construction where it is used; one rule for every new claim | `ws::before` and `deadline_first` keep their own hand-written orders | yes | one module, two call sites |
| Sample inside a `poll_fn` while `fut` is alive | one wrapper | samples before `fut`'s destructor: a destructor that runs past the deadline would no longer turn the result late | yes | same |
| A second, check-before-poll entry point for WebSocket and coordinator waits | all deadline waits in one module | moves the clock sample relative to `ws::before`'s timer poll, and changes the registry's poll order and notification consumption | yes | more call sites, each needing its own order argument |
| Keep the three local rules, add only the lint | smallest diff | no helper for the next claim site | yes | low now, recurring later |
| Shared crate | usable outside the daemon | `crates/core` is Sans-IO; `pty-runtime` and the CLI have no tokio in production | yes (moving is mechanical) | new workspace member |

**Home.** `crates/daemon/src/deadline.rs`, daemon-local: the daemon is the only crate with async production code.

### D2. The lint

`crates/daemon/clippy.toml` gains:

```toml
{ path = "tokio::time::timeout", reason = "decide through deadline::settle_before, or a named exception fn" },
{ path = "tokio::time::timeout_at", reason = "decide through deadline::settle_before, or a named exception fn" },
{ path = "tokio::time::sleep_until", reason = "a wait to a deadline: deadline::settle_before, or a named exception fn" },
{ path = "tokio::time::sleep", reason = "a relative wait can express a deadline: deadline::settle_before, or a named exception fn" },
```

- **Where.** In the daemon's own `clippy.toml`, not a workspace-root one: clippy reads the nearest `clippy.toml` above the crate's manifest and does not merge it with a parent's, so a root file is shadowed by the existing `crates/{core,cli,daemon}/clippy.toml`. Verified with a scratch crate (deleted): a parent's list applied to a child without its own file, and was ignored once the child had one.
- **Coverage.** The scratch crate showed the lint firing on a `use`-imported bare call, the full path, a module alias, a crate re-export, the function taken as a value (as in `deadline.map(sleep_until)`), inside `#[cfg(test)]` modules, and in integration tests under `--all-targets`; an `allow` with a reason on a function suppressed it. `Sleep::reset`, `interval`, `interval_at`, and timers inside libraries (hyper's `header_read_timeout`) stay outside the lint; review owns them.
- **Exception scope.** `clippy::disallowed_methods` also carries the router and spawn-lock entries, so no `allow` covers a module, a file, a `select!`, or a function that does more than construct one timer. Each exception is a private function whose body is the one tokio call, with `#[allow(clippy::disallowed_methods, reason = "…")]` naming the contract its callers follow (the repository's form: `routes::build_router`, `lifecycle::spawn_locked`). Each call site calls the function where it calls tokio today, so the timer is constructed at the same point with the same arguments, including Tokio's relative `timeout` with its checked add at the hand-over.
- **Production exceptions (13).**

  | Function | Wraps | Callers and the contract in the reason |
  |---|---|---|
  | `deadline::bound` | `timeout_at` | `settle_before` (D1) |
  | `ctx::reply_cutoff` | `timeout_at` | `await_write_releasing`: a closed reply channel is 503 before any clock reading; a reply is classified by one sample (http-surface §2.3) |
  | `commands::reply_wake` | `timeout_at` | `await_reply`: wakes the loop; the progress read and its sample decide (§2.8) |
  | `commands::backoff_until` | `sleep_until` | completion backoff; the budget is read before each attempt (§2.8) |
  | `ws::socket_timer` | `sleep_until` | `before` (timer, clock check, then poll: §2.6 "Bounded disconnect"), and the startup and loop `select!` arms (a fired timer drops the socket or ticks; results go through `take_read` or `before`, §2.6) |
  | `hook::pid_timer` | `sleep_until` | pid wait: biased timer, then one sample and a strict re-read (hook-ingest §2.3) |
  | `registry::drain_timer` | `sleep_until` | `wait_empty_until`'s unbiased `select!` (§2.2 "Shutdown") |
  | `upgrade::hand_over_bound` | `timeout` | hand-over, polled socket first (§2.4) |
  | `shutdown::deadline_timer` | `sleep_until` | `deadline_first`: clock, poll, timer (daemon-lifecycle §2.8) |
  | `shutdown::participant_bound` | `timeout_at` | participants, polled first, result discarded (§2.8) |
  | `shutdown::record_pause` | `sleep` | record-removal retry; the deletion is admitted by the budget checks (§2.8 step 5) |
  | `listener::accept_backoff` | `sleep` | accept-error backoff, shutdown wins (§2.2) |
  | `listener::stall_timer` | `sleep` | `ConnIo` write stall, progress first (§2.2) |

- **Tests** bound and pause through one module, `crates/daemon/tests/support/waits.rs` (`pub mod waits;` in `tests/support/mod.rs`). That support tree is compiled into the integration tests as `support::waits` and into the unit tests as `crate::store::tests::support::waits` (through the existing `#[path]`):

  ```rust
  // Test bounds and pauses only; a deadline claim under test is checked with paused time,
  // an injected clock, or the fault gates, never with these as its oracle.
  #[allow(clippy::disallowed_methods, reason = "…")]
  pub fn bounded<F: Future>(within: Duration, fut: F) -> tokio::time::Timeout<F>;            // tokio::time::timeout
  #[allow(clippy::disallowed_methods, reason = "…")]
  pub fn bounded_at<F: Future>(at: tokio::time::Instant, fut: F) -> tokio::time::Timeout<F>; // tokio::time::timeout_at
  pub fn pause(d: Duration) -> impl Future<Output = ()>;                    // bounded(d, pending()) built now
  pub fn pause_until(at: tokio::time::Instant) -> impl Future<Output = ()>; // bounded_at(at, pending()) built now
  ```

  They are plain functions, not `async fn`, so a deadline is computed when the call is evaluated, as Tokio's constructors compute it; `pause` and `pause_until` build their `Timeout` before returning the future that awaits it. Two `allow`s, both in this file. No test uses a `Sleep` method (`reset`, `deadline`), so `impl Future` suffices. The migration is mechanical: `tokio::time::timeout(` → `waits::bounded(`, `timeout_at(` → `waits::bounded_at(`, `sleep_until(` → `waits::pause_until(`, `sleep(` → `waits::pause(`, plus one `use` per file. Count (`rg -o 'tokio::time::(timeout_at|timeout|sleep_until|sleep)\b'` under `tests/` paths): **74 call sites in 22 files** (47 `timeout`, 3 `timeout_at`, 16 `sleep_until`, 8 `sleep`): 40 in 10 unit-test files (`src/hook/tests/{handler,intake}.rs`, `src/http/tests/{commands,faults,layers,reads,registry,upgrade,writes,ws}.rs`) and 34 in 12 integration files (`hook_b8`, `hook_ingest`, `http_caps`, `http_commands`, `http_contract`, `http_events`, `http_layers`, `http_matrix`, `http_server`, `http_ws`, `support/commands.rs`, `support/http.rs`). No other crate's tests use these functions.
- **The check that the ban fires** (D4).

### D3. Migration scope

**Migrated**, identical by construction:

| Site | Becomes | Why identical |
|---|---|---|
| `ctx::read_cut` (`:71`) | `settle_before(server now, deadline, cutoff, handle)`: `Elapsed` → 408; `InTime(j)` / `Late(j)` → `after_cutoff_sample`, then `read_outcome` (`classify(false` / `true, strict, j)`) | the same `timeout_at(cutoff, handle)` on the same `cutoff`, the same drop point (the end of its `.await`), the same single sample (`passed` is `clock.now() >= deadline`), the same fault-point order, the same `classify` call |
| `ctx::until_deadline` (`:146`) | `settle_before(server now, deadline, cutoff, start())`, then `in_time()`, `None` → 408 | the same `timeout_at(cutoff, start())` with `start()` evaluated at the same point; `Err` reads no clock (the guarded arm `Ok(v) if !passed` does not run for `Err`); `Ok` reads it once, after the drop |

`hook/mod.rs:243`, `:262` and the guard follow through `until_deadline`. The `ctx.rs` comments saying the cutoff is set before the read or `start()` stay true.

**Kept**, each on today's code with its timer constructed by its D2 exception function: `ctx::await_write_releasing` (its closed-channel `Ok(Err(_))` → 503 arm reads no clock, and the helper samples every `Ok`), `ws::before` and the WebSocket `select!`s, `shutdown::deadline_first`, the upgrade hand-over, the shutdown participants, `registry::wait_empty_until`, `hook::pid_wait`, `commands::await_reply`'s wake, and the backoff, record-removal, accept, and stall timers. A later PR may move one of them to `settle_before` only with its own design for the poll, sample, and drop order it changes.

**Also in this PR (no runtime change):** `HttpShutdownReport::exited`'s rustdoc is corrected to what the code counts (F2).

**Documents.** The implementing PR names `settle_before` in http-surface §2.3 where it names `timeout_at` plus the sample. daemon-lifecycle §2.8 already describes the coordinator's waits as coded.

### D4. Test strategy

All deterministic: a counting test clock (a closure over a cell), futures built with `poll_fn` that count their polls, paused tokio time for the timer; no real-time sleeps.

**The helper** (unit tests in `deadline.rs`):

| Claim | Test | Mutation it kills |
|---|---|---|
| ready before the deadline → `InTime`, one sample | clock one tick before the deadline; ready on first poll; the clock counts exactly one reading | an extra sample, or none |
| ready at the deadline → `Late(v)` | the future's poll sets the clock to exactly the deadline, then returns `Ready` | `>=` → `>`; the sample removed |
| ready but first polled after the deadline → `Late(v)`, polled once | clock past the deadline before the first poll | a check before the poll added |
| the sample follows the drop | a future ready while the clock is before the deadline, whose `Drop` sets the clock to the deadline → `Late(v)` | sampling while `fut` is alive (inside a `poll_fn`) |
| the timer ends a pending wait, with no reading | paused time; the waiter in its own spawned task, the watchdog (`waits::bounded`) in the test task; the logical clock held before the deadline; `timer` = tokio now + 10 s; a never-ready future → `Elapsed`, `tokio::time::Instant::now() == timer` when the helper returns, clock reading count 0 | timer not polled; timer taken from the logical deadline; a sample added to `Elapsed` |
| `in_time` | each variant | the mapping |

**Migrated sites.**

- `read_outcome(settled, strict)` table: `InTime(Ok(Ok(v)))` → `v`; `Late(Ok(Ok(v)))` → 408; `Late(Ok(Err(Store(DiskFull))))` → 408; `Late(Ok(Err(ReadError::Busy)))` → 503 `busy` without `strict`, 408 with it; `InTime(Ok(Err(ReadError::Busy)))` → 503 both ways; `Late(Err(join))` → 408; `InTime(Err(join))` → 500; `Elapsed` → 408.
- `until_deadline` end to end with a supplied future ready before the deadline whose `Drop` skews the `SkewClock` past it → 408 (`http::tests::reads::until_deadline_drop_past_the_deadline_is_408`); kills sampling before the drop and a mapping that admits `Late`.
- Existing tests stay unchanged and must pass. They reach the `Late` path end to end, so they also kill wiring that bypasses `read_outcome` or `in_time()`: `http::tests::reads::{late_read_success_is_408, late_read_error_is_408, late_body_is_408, late_admission_read_keeps_the_ticket, read_by_cutoff_precedes_the_outcome, read_by_classifies_with_one_sample}` (the last proves a result taken in time stays accepted after a pause) and `http::tests::writes::body_after_the_cutoff_submits_nothing`.

**Kept sites** keep their code, so their existing tests are unchanged; among them `http::tests::writes::reply_after_the_grace_is_outcome_unknown` (plain write), `hook::tests::handler::pid_wait_timings`, the `http::tests::commands` reply-time tests, `http::tests::ws`, `http::tests::upgrade`, and `lifecycle::tests::helpers`. Tests that would pin finer details of those sites are follow-up F6.

**The lint.** CI runs `cargo clippy --workspace --all-targets --locked -- -D warnings` on macOS and Windows, so a direct call without an exception fails it. A membership check alone would miss a lint that stops firing (a renamed path, a shadowing `clippy.toml`), so a probe exercises the ban:

- **The probe is a test target of the daemon**: `crates/daemon/tests/lint_probe.rs` holds the four banned calls (one through a `use` import) in a never-called `#[allow(dead_code)] async fn`, so the target has no test to run. `crates/daemon/Cargo.toml` declares `[features] lint-probe = []` (enables nothing) and `[[test]] name = "lint_probe"`, `required-features = ["lint-probe"]`. It shares the workspace's `Cargo.lock` and target directory, needs nothing beyond what the workspace fetches, and is skipped by `cargo clippy --workspace --all-targets` and `cargo test --workspace`. `--all-features` (or `--features lint-probe`) selects it, and clippy then fails on it by design; neither CI nor lefthook passes those flags.
- **The command**, in `scripts/lint-probe.sh` (bash): `cargo clippy --locked -p chaldea-daemon --features lint-probe --test lint_probe --color never --message-format short -- -D warnings`. Short, uncoloured output gives one line per diagnostic with its primary span first and needs no JSON parser on either OS; CI's `CARGO_TERM_COLOR: always` would otherwise wrap `error` in escape codes. The script strips trailing CRs and turns `\` into `/`, then requires: a non-zero exit; exactly four lines of the form `<path>:<line>:<col>: error: …`; and, for each of the four names, exactly one ``crates/daemon/tests/lint_probe.rs:<line>:<col>: error: use of a disallowed method `tokio::time::<name>` `` line. Removing one ban leaves three, removing all leaves a zero exit, and an unrelated error either stops the lib before the probe or adds a fifth line; each fails the probe. A change of clippy's message wording also fails it, loudly.
- **What it rebuilds.** Measured locally on a warm workspace: with the same `-- -D warnings` as the clippy step, only `chaldea-daemon` is re-checked (its feature set changed); every third-party crate, `chaldea-core`, and `chaldea-pty-runtime` stay fresh. Different arguments after `--` would re-check the other workspace crates as well. The re-check took under 2 s while the daemon's lib failed fast on the not-yet-excepted sites; a passing lib check plus the probe is expected to add about the daemon's own clippy time, under 30 s on the CI runners.
- **Where it runs.** A separate step, "Probe the timer ban", right after "Run clippy" in the `workspace` job on both macOS and Windows (the job's default shell is bash, so Windows runs it in Git Bash), reusing that step's check artifacts. It is not a test inside `cargo test`: a nested cargo would contend for the target directory's lock or need a second target directory, and the pre-push hook's `cargo test` runs without a prior clippy, so the nested check would start cold. lefthook's pre-commit runs the same script as `daemon-lint-probe` after `workspace-clippy`, with the globs `crates/daemon/clippy.toml`, `crates/daemon/Cargo.toml`, `crates/daemon/tests/lint_probe.rs`, `scripts/lint-probe.sh`, `Cargo.{toml,lock}`, and `rust-toolchain.toml`.
- `crates/cli/tests/lints.rs`'s membership parse also asserts the four paths, so a deleted entry fails with a precise message.

## 4. Purpose Layer and Plan Layer

### 4.1 Purpose Layer: Acceptance Criteria

| ID | Criterion (EARS) | Source | How checked |
|---|---|---|---|
| DL-P-1 | When a read's result, or a guarded handler's or body's future, is first observed at or after its deadline, including a future whose destructor runs past it, the daemon shall answer 408, except that a non-strict read's reader-pool exhaustion answers 503 `busy`. | http-surface §2.3; daemon-lifecycle §2.10 "Late results" | DL-CL-1 to DL-CL-5 |
| DL-P-2 | If a change adds a direct call to `tokio::time::timeout`, `timeout_at`, `sleep`, or `sleep_until` in any target of `crates/daemon` outside a function that carries its own `allow`, then CI shall fail. | §1 | CI clippy step; DL-CL-6 |
| DL-P-3 | The daemon's HTTP status codes, reply bodies, WebSocket behaviour, hand-over, ticket consumption, cancellation of cut-off work, and shutdown steps shall stay as their designs state. | http-surface, daemon-lifecycle, hook-ingest | the full suite passes with no test edited except the mechanical `waits` replacement and added tests |

### 4.2 Plan Layer: Claims

| ID | Claim (EARS) | Source | Falsifying test |
|---|---|---|---|
| DL-CL-1 | When the future completes, `settle_before` shall drop it and then classify its output by one clock sample: `Late(v)` at or after the deadline, `InTime(v)` before it. | D1 | unit `deadline::tests::ready_in_time_takes_one_sample`, `…::ready_at_the_deadline_is_late`, `…::polled_first_after_the_deadline_is_late`, `…::sample_follows_the_drop` |
| DL-CL-2 | While the future is pending, `settle_before` shall return `Elapsed` when the caller's `timer` instant passes, without reading the clock. | D1 | unit `deadline::tests::timer_ends_a_pending_wait` |
| DL-CL-3 | When a read's result is `Late`, `read_cut` shall answer 408, except reader-pool exhaustion on a non-strict read, which answers 503 `busy`. | http-surface §2.3 | unit `http::ctx::tests::read_outcome_table`; `http::tests::reads::{late_read_success_is_408, late_read_error_is_408, read_by_cutoff_precedes_the_outcome}` |
| DL-CL-4 | When a read's result is taken before its deadline, `read_cut` shall keep it even if the waiter resumes after the deadline. | http-surface §2.3 | `http::tests::reads::read_by_classifies_with_one_sample` |
| DL-CL-5 | When the future of `until_deadline` is first observed at or after its deadline, or its destructor runs past it, `until_deadline` shall answer 408. | http-surface §2.3 | `http::tests::reads::{until_deadline_drop_past_the_deadline_is_408, late_body_is_408}`, `http::tests::writes::body_after_the_cutoff_submits_nothing` |
| DL-CL-6 | Clippy shall reject a direct call to each of `tokio::time::{timeout, timeout_at, sleep, sleep_until}` in a target of `crates/daemon` that carries no `allow`. | D2 | CI step "Probe the timer ban" (`scripts/lint-probe.sh`); `cli/tests/lints.rs::clippy_tomls_list_the_bans` |
| DL-CL-7 | The kept sites shall keep their code, except that each tokio timer call becomes a call of its exception function at the same point with the same arguments. | D3 | review of the diff; their existing tests (D4) |

## 5. Out of Scope

- The synchronous helpers of `pty-runtime::host`, the CLI's `poll_until`, ticket and pair-code expiry, and the store's blocking waits.
- hyper's `header_read_timeout`, `Sleep::reset`, `interval` (review-only).
- Moving the kept sites onto `settle_before`.
- The CLI crate's `clippy.toml`: the CLI has no tokio outside its tests.

## 6. Follow-ups

Pre-existing mismatches and deferred work, not changed here:

- **F1.** Record removal: the retry pause is relative and computed before its first poll, so a pause between the two can carry the wait past the 500 ms budget, against daemon-lifecycle §2.8 step 5 ("the wait before a retry never runs past the budget"). No deletion follows the budget (it is checked before each deletion). Fix: an absolute wake instant.
- **F2.** HTTP drain report: `exited` is the closing snapshot minus the tasks still registered at the abort pass, so a task that exits between the deadline and `abort_remaining()` counts as `exited`, as http-surface §2.2 step 4 counts it; `HttpShutdownReport::exited`'s rustdoc says "ended before the deadline". This PR corrects the rustdoc (D3).
- **F3.** WebSocket idle: renewal follows the idle timer's readiness, not a strict cutoff (§2.1). http-surface §2.6 says a socket "with no inbound frame for `idle_timeout`" is dropped. Decide whether a frame read after the idle instant but before the timer fires may renew.
- **F4.** `ConnIo` write stall is progress first: ready I/O after a pause clears a due stall timer. Decide whether expiry must win.
- **F5.** `until_deadline` builds the handler's future before its first poll; moving construction into an admitted poll would make the guard's admission and the construction one step.
- **F6.** Tests pinning current behaviour of kept sites: the hand-over serving a socket queued before the bound (the producer acknowledging its `tx.send`, then paused time past the recorded bound), a participant first polled after an already expired bound, an admitted WebSocket send crossing its deadline, the pid wait's post-wake sample on a matching commit with the logical cutoff passed and the timer pending, and no clock reading inside the reply wake.
