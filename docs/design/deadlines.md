# Design: One Deadline Rule for Async Waits

Status: implemented in `crates/daemon/src/deadline.rs`, `crates/daemon/clippy.toml`, and `scripts/lint-probe.sh`. Scope: the daemon's async waits that end at a deadline. The synchronous side has its own rule (`docs/design/daemon-lifecycle.md` §2.10 "Deadlines of start and stop", the four helpers of `pty-runtime::host`). This document gives the async side one helper for the claim shape the HTTP reads use, and a lint that puts every raw tokio timer in the daemon behind that helper or behind a small, named, reviewed exception.

This is its own document rather than a section of `http-surface.md` because the rule spans three designs (HTTP, hook ingest, lifecycle) and the lint covers the whole daemon crate.

## 1. Purpose

The defect this design guards against is a completion racing its deadline. `tokio::time::timeout_at` polls the inner future before its timer, so a result that became ready after the deadline is still delivered; a `select!` against a sleep has the same race unless something samples the clock after the result.

- `deadline::settle_before` is the `ctx` shape as a function: await an owned `timeout_at`, then classify a ready result with one clock sample. Its return type makes each caller say whether a late result counts.
- `crates/daemon/clippy.toml` forbids `tokio::time::{timeout, timeout_at, sleep, sleep_until}`. **The guarantee** is what clippy enforces: no direct call of a banned function in any scope where `clippy::disallowed_methods` is active, outside the named exception functions, the helper, and the test wrappers. A new deadline decision either uses `settle_before` or adds a reviewed exception. A new caller of an existing exception function adds no `allow`; review owns those callers. Two functions allow the lint for other APIs, `routes::build_router` (the router) and `lifecycle::spawn_locked` (the spawn lock), so a timer call inside them would not be caught; neither contains one. The probe's four raw calls (D4) are the negative fixture and are not part of this inventory.
- Only the two `ctx` sites in the helper's shape use it; every other site (a **kept site**) keeps its own poll order, with its timer constructed by a private exception function.

## 2. The three poll orders

| Order | Check before poll | Sample after `Ready` | Clock | Used by |
|---|---|---|---|---|
| `settle_before` (owned `timeout_at`, then one sample) | no | yes, one sample classifies | the caller's (`MonotonicClock`, injectable) | `ctx::read_cut`, `ctx::until_deadline` |
| `ws::before` | yes | no | tokio clock | WebSocket sends (http-surface §2.6 "Bounded disconnect") |
| `shutdown::deadline_first` | yes | no | std clock | the shutdown coordinator (daemon-lifecycle §2.8) |

Each serves a different contract: claims need the sample after the result; actions (a WebSocket send hands bytes to the transport on every poll) need the check before each poll; lifecycle bounds accept "a poll admitted before the cutoff can return `Ready`". Moving the second or third onto a shared helper would move a clock sample, a poll, or a drop relative to the others, so they stay hand-written behind their exception functions (D2).

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

Step 3 is §2.10's "each return is classified with one clock sample", and `in_time()` follows §2.10 for a result first observed at or after the deadline, including one produced earlier and left queued. The sample follows the drop, so a future whose destructor runs past the deadline is classified by the later time; sampling inside a `poll_fn` while `fut` is alive would lose that.

**Clock and timer.** `now` is the clock that decides (the server's `MonotonicClock::now` at both call sites). `timer` is a tokio instant the caller computes before the work starts (`Shared::timer(deadline)` before the read is spawned, before `start()`), because `MonotonicClock::real` depends on the clock's state. The helper never converts and holds no `MonotonicClock`.

**Ownership.** On `Elapsed` the `Timeout` has dropped `fut`: a `JoinHandle` detaches its task (a cut-off read finishes on its own, http-surface §2.3). On `Late(v)` the caller owns `v` and drops it (a read result, a blob permit inside it).

**Poll first.** A second, check-before-poll entry point (`admit_before`) for the WebSocket and coordinator waits was rejected: it would move the clock sample relative to `ws::before`'s timer poll and change the registry's poll order and notification consumption. Those waits keep their own orders (§2).

**Home.** `crates/daemon/src/deadline.rs`, daemon-local: the daemon is the only crate with async production code, and `crates/core` is Sans-IO.

### D2. The lint

`crates/daemon/clippy.toml` lists the four functions under `disallowed-methods`, each with a reason naming `deadline::settle_before` or a named exception function.

- **Where.** In the daemon's own `clippy.toml`: clippy reads the nearest `clippy.toml` above the crate's manifest and does not merge it with a parent's, so a workspace-root file would be shadowed by `crates/{core,cli,daemon}/clippy.toml`.
- **Coverage.** The lint fires on a `use`-imported bare call, the full path, a module alias, a crate re-export, the function taken as a value (as in `deadline.map(sleep_until)`), inside `#[cfg(test)]` modules, and in integration tests under `--all-targets`. `Sleep::reset`, `interval`, `interval_at`, and timers inside libraries (hyper's `header_read_timeout`) are outside it; review owns them.
- **Exception scope.** `clippy::disallowed_methods` also carries the router and spawn-lock entries, so an `allow` covers only a private function whose body is the one tokio call, with `#[allow(clippy::disallowed_methods, reason = "…")]` naming the contract its callers follow (the repository's form: `routes::build_router`, `lifecycle::spawn_locked`). Each call site calls the function where it would call tokio, so the timer is constructed at that point with the same instant or duration, including Tokio's relative `timeout` with its checked add at the hand-over; `listener::accept_backoff` takes no argument and holds its constant 1 s inside.
- **Production exceptions (14).**

  | Function | Wraps | Callers and the contract in the reason |
  |---|---|---|
  | `deadline::bound` | `timeout_at` | `settle_before` (D1) |
  | `ctx::reply_cutoff` | `timeout_at` | `await_write_releasing`: a closed reply channel is 503 `outcome_unknown` before any clock reading; a reply is classified by one sample against `reply_by` (http-surface §2.3). The cutoff is converted before the observer registers and the job is submitted. |
  | `commands::reply_wake` | `timeout_at` | `await_reply`: wakes the loop; the progress read and its sample decide (§2.8) |
  | `commands::backoff_until` | `sleep_until` | completion backoff; the budget is read before each attempt (§2.8). Re-exported from `http` with the rest of the completion policy and shared with the supervisor, whose retried writes (`supervisor::record`) follow the same contract. |
  | `ws::socket_timer` | `sleep_until` | `before` (timer, clock check, then poll: §2.6 "Bounded disconnect"), and the startup and loop `select!` arms (a fired timer drops the socket or ticks; results go through `take_read` or `before`, §2.6). The startup `select!` is `biased`: shutdown, then the timer, then the startup read's join. |
  | `hook::pid_timer` | `sleep_until` | pid wait: biased timer, then one sample and a strict re-read (hook-ingest §2.3) |
  | `registry::drain_timer` | `sleep_until` | `wait_empty_until`'s unbiased `select!` (§2.2 "Shutdown") |
  | `upgrade::hand_over_bound` | `timeout` | hand-over, polled socket first (§2.4) |
  | `shutdown::deadline_timer` | `sleep_until` | `deadline_first`: clock, poll, timer (daemon-lifecycle §2.8) |
  | `shutdown::participant_bound` | `timeout_at` | participants, polled first; a result taken at or after the participant's end counts as not done by it, which keeps the lock (§2.8 steps 3 and 6) |
  | `shutdown::record_pause` | `sleep` | record-removal retry; the deletion is admitted by the budget checks (§2.8 step 5) |
  | `listener::accept_backoff` | `sleep` | accept-error backoff, shutdown wins (§2.2) |
  | `listener::stall_timer` | `sleep` | `ConnIo` write stall, progress first (§2.2) |
  | `supervisor::tick_timer` | `sleep_until` | the supervisor's 1 s tick period; each tick decides from the store (`docs/design/supervisor.md` §2.6) |

  Moving one of these sites to `settle_before` needs its own design for the poll, sample, and drop order it changes.

- **Tests** bound and pause through `crates/daemon/tests/support/waits.rs`, compiled into the integration tests as `support::waits` and into the unit tests as `crate::store::tests::support::waits` (through the existing `#[path]`). `bounded` and `bounded_at` wrap `timeout` and `timeout_at` and carry the file's two `allow`s; `pause` and `pause_until` are `bounded(d, pending())` and `bounded_at(at, pending())` with the result dropped. All four are plain functions, not `async fn`, so a deadline is computed when the call is evaluated, as Tokio's constructors compute it. They return `Timeout` or `impl Future`; a test that needs a `Sleep` method (`reset`, `deadline`) needs a new wrapper. A deadline claim under test is checked with paused time, an injected clock, or the fault gates, never with these as its oracle.

### D3. Call sites

| Site | Code | Why it fits the helper |
|---|---|---|
| `ctx::read_cut` | `settle_before(server now, deadline, cutoff, handle)`; for `InTime` / `Late` the test-only `after_cutoff_sample` gate runs next; then `read_outcome(settled, strict)`: `Elapsed` → 408; `InTime(j)` / `Late(j)` → `classify(false` / `true, strict, j)` | one `timeout_at(cutoff, handle)`, the handle dropped at the end of its `.await`, one sample (`clock.now() >= deadline`) |
| `ctx::until_deadline` | `settle_before(server now, deadline, cutoff, start())`, then `in_time()`, `None` → 408 | `start()` is evaluated, and `cutoff` converted, before the await; `Err` reads no clock; `Ok` reads it once, after the drop |

`hook/mod.rs` and the guard reach the helper through `until_deadline`. Every kept site keeps its own order with its D2 exception function; `ctx::await_write_releasing` stays out because its closed-channel `Ok(Err(_))` → 503 `outcome_unknown` arm reads no clock, while the helper samples every `Ok`.

### D4. Tests and the probe

All deterministic: a counting test clock, futures built with `poll_fn` that count their polls, paused tokio time for the timer; no real-time sleeps.

**The helper** (unit tests in `deadline/tests.rs`):

| Claim | Test | Mutation it kills |
|---|---|---|
| ready before the deadline → `InTime`, one sample | clock one tick before the deadline; ready on first poll; the clock counts exactly one reading | an extra sample, or none |
| ready at the deadline → `Late(v)` | the future's poll sets the clock to exactly the deadline, then returns `Ready` | `>=` → `>`; the sample removed |
| ready but first polled after the deadline → `Late(v)`, polled once | clock past the deadline before the first poll | a check before the poll added |
| the sample follows the drop | a future ready while the clock is before the deadline, whose `Drop` sets the clock to the deadline → `Late(v)` | sampling while `fut` is alive (inside a `poll_fn`) |
| the timer ends a pending wait, with no reading | paused time; the waiter in its own spawned task, the watchdog (`waits::bounded`, 60 s) in the test task, strictly later than both instants below; both instants derived from one sample of the paused clock: the logical clock held at it, its deadline 5 s on, distinct from `timer` 10 s on; a never-ready future → `Elapsed`, `tokio::time::Instant::now() == timer` when the helper returns, clock reading count 0 | timer not polled; timer taken from the logical deadline; a sample added to `Elapsed` |
| `in_time` | each variant | the mapping |

**Call sites.**

- `read_outcome(settled, strict)` table (`http::ctx::tests::read_outcome_table`): `InTime(Ok(Ok(v)))` → `v`; `Late(Ok(Ok(v)))` → 408; `Late(Ok(Err(Store(DiskFull))))` → 408; `Late(Ok(Err(ReadError::Busy)))` → 503 `busy` without `strict`, 408 with it; `InTime(Ok(Err(ReadError::Busy)))` → 503 both ways; `Late(Err(join))` → 408; `InTime(Err(join))` → 500; `Elapsed` → 408.
- `http::tests::reads::until_deadline_drop_past_the_deadline_is_408`: a supplied future ready before the deadline whose `Drop` skews the `SkewClock` past it → 408; kills sampling before the drop and a mapping that admits `Late`.
- `http::tests::reads::late_pool_exhaustion_on_a_plain_read_is_busy`: every reader held, the waiter held at the cutoff gate until the pool's wait has ended at the deadline with `ReadError::Busy`, then released; it takes the result ready on its first poll, so the sample is past the deadline → 503 `busy`. Kills `Late` mapped without `strict` (as 408) and a check before the poll.
- `http::tests::reads::{late_read_success_is_408, late_read_error_is_408, late_body_is_408, late_admission_read_keeps_the_ticket, read_by_cutoff_precedes_the_outcome, read_by_classifies_with_one_sample}` reach the `Late` path end to end, so they also kill wiring that bypasses `read_outcome` or `in_time()` (the last proves a result taken in time stays accepted after a pause). `http::tests::writes::body_after_the_cutoff_submits_nothing` does not catch a `Late` body accepted by `until_deadline`, since the write's own admission check then answers the same 408 with nothing submitted.

The kept sites keep their own tests, among them `http::tests::writes::reply_after_the_grace_is_outcome_unknown`, `hook::tests::handler::pid_wait_timings`, the `http::tests::commands` reply-time tests, `http::tests::ws`, `http::tests::upgrade`, and `lifecycle::tests::helpers`; tests pinning finer details are follow-up F6.

**The lint.** CI runs `cargo clippy --workspace --all-targets --locked -- -D warnings` on macOS and Windows, so a direct call without an exception fails it. `crates/cli/tests/lints.rs::clippy_tomls_list_the_bans` asserts the four paths, so a deleted entry fails with a precise message. A membership check alone would miss a lint that stops firing (a renamed path, a shadowing `clippy.toml`), so a probe exercises the ban:

- **Fixture.** `crates/daemon/tests/lint_probe.rs` holds the four banned calls (one through a `use` import) in a never-called `#[allow(dead_code)] async fn`. `crates/daemon/Cargo.toml` declares `[features] lint-probe = []` and `[[test]] name = "lint_probe"`, `required-features = ["lint-probe"]`, so `cargo clippy --workspace --all-targets` and `cargo test --workspace` skip it, and `--all-features` (or `--features lint-probe`) selects it, and clippy then fails on it by design; neither CI nor lefthook passes those flags.
- **Command**, in `scripts/lint-probe.sh`: `cargo clippy --locked --workspace --features chaldea-daemon/lint-probe --test lint_probe --message-format json -- -D warnings`. Selecting the workspace, with the feature named through the package, resolves the same dependency features as the clippy step on both targets, so only `chaldea-daemon` is re-checked; `-p chaldea-daemon` alone resolves `windows-sys` for tokio with fewer features on Windows and would check tokio a second time. The arguments after `--` must match the clippy step's, or the other workspace crates are re-checked too.
- **Verdict**, from cargo's JSON messages on stdout, parsed with `jq` (pinned in `mise.toml` and `mise.lock`). Stderr is discarded, so cargo's human output and its verbosity do not matter; the JSON messages are the contract, where a text matcher depended on cargo's human wording. The probe requires: cargo's exit status exactly 101; exactly one `build-finished` record, with `success: false`; exactly four `compiler-message` records of an error level (`error`, or `error: internal compiler error`) in the whole build; each in the `lint_probe` target, with code `clippy::disallowed_methods` and one primary span whose `file_name`, with `\` turned into `/`, ends in `crates/daemon/tests/lint_probe.rs` at a path boundary; and their messages naming each banned function exactly once. The function is read from the message, which names the resolved path however the call is spelled; the span's text would show only `sleep_until` for the imported call. Removing one ban leaves three errors, removing all leaves exit 0 and a successful build, and any other error changes the count, the target, the code, or the span; each fails the probe, and so does any other exit status (cargo killed after printing).
- **Coverage limit.** A compiler child killed after printing all four messages is reported by cargo only on stderr while it still exits 101 with an unsuccessful build, so the probe passes it. The workspace clippy step does not compile the `lint_probe` target, so no other step reports that crash either.
- **Script tests.** `scripts/lint-probe.sh --check STATUS` checks JSON messages read from stdin as if cargo had exited with `STATUS`. The repository has no place for script tests, so sample runs are not committed.
- **Where it runs.** CI's `workspace` job on macOS and Windows (default shell bash, Git Bash on Windows) runs the step "Probe the timer ban" after "Run clippy", reusing its check artifacts, with a pinned `jdx/mise-action` step and `mise install --locked jq` between them; the step runs `mise exec jq -- bash scripts/lint-probe.sh`, which activates only jq. The probe stays out of `cargo test`: a nested cargo would contend for the target directory's lock or need a second target directory, and the pre-push hook's `cargo test` runs without a prior clippy, so the nested check would start cold. lefthook's pre-commit runs the same command as `workspace-clippy-probe`. lefthook runs commands in name order, so when `workspace-clippy` also runs, the probe follows it and reuses its artifacts; its own globs (`crates/daemon/clippy.toml`, `crates/daemon/Cargo.toml`, `crates/daemon/tests/lint_probe.rs`, `scripts/lint-probe.sh`, `lefthook.yml`, `mise.{toml,lock}`, `Cargo.{toml,lock}`, `rust-toolchain.toml`) run it without `workspace-clippy` only when none of `workspace-clippy`'s globs also match; several overlap (`crates/daemon/clippy.toml`, `crates/daemon/Cargo.toml`, `crates/daemon/tests/lint_probe.rs`, `Cargo.{toml,lock}`, `rust-toolchain.toml`).

## 4. Purpose Layer and Plan Layer

### 4.1 Purpose Layer: Acceptance Criteria

| ID | Criterion (EARS) | Source | How checked |
|---|---|---|---|
| DL-P-1 | When a read's result, or a guarded handler's or body's future, is first observed at or after its deadline, including a future whose destructor runs past it, the daemon shall answer 408, except that a non-strict read's reader-pool exhaustion answers 503 `busy`. | http-surface §2.3; daemon-lifecycle §2.10 "Late results" | DL-CL-1 to DL-CL-5 |
| DL-P-2 | If a change adds a direct call to `tokio::time::timeout`, `timeout_at`, `sleep`, or `sleep_until` in `crates/daemon` outside a function that carries its own `allow`, in code that CI's clippy configurations compile (every target with default features, on macOS and Windows), then CI shall fail. Daemon library code compiled only under `cfg(all(test, feature = "lint-probe"))` is in neither configuration: the probe builds the daemon library as a dependency of its target, without `cfg(test)`. | §1 | CI clippy step; DL-CL-6 |
| DL-P-3 | The change shall preserve the daemon's current runtime behaviour: HTTP status codes, reply bodies, WebSocket behaviour, hand-over, ticket consumption, cancellation of cut-off work, and shutdown steps (the pre-existing mismatches F1, F3, F4 stay). | http-surface, daemon-lifecycle, hook-ingest | the full suite passes with no test edited except the mechanical `waits` replacement and added tests |

### 4.2 Plan Layer: Claims

| ID | Claim (EARS) | Source | Falsifying test |
|---|---|---|---|
| DL-CL-1 | When the future completes, `settle_before` shall drop it and then classify its output by one clock sample: `Late(v)` at or after the deadline, `InTime(v)` before it. | D1 | unit `deadline::tests::ready_in_time_takes_one_sample`, `…::ready_at_the_deadline_is_late`, `…::polled_first_after_the_deadline_is_late`, `…::sample_follows_the_drop` |
| DL-CL-2 | While the future is pending, `settle_before` shall return `Elapsed` when the caller's `timer` instant passes, without reading the clock. | D1 | unit `deadline::tests::timer_ends_a_pending_wait` |
| DL-CL-3 | When a read's result is `Late`, `read_cut` shall answer 408, except reader-pool exhaustion on a non-strict read, which answers 503 `busy`. | http-surface §2.3 | unit `http::ctx::tests::read_outcome_table`; `http::tests::reads::{late_read_success_is_408, late_read_error_is_408, read_by_cutoff_precedes_the_outcome, late_pool_exhaustion_on_a_plain_read_is_busy}` |
| DL-CL-4 | When a read's result is taken before its deadline, `read_cut` shall keep it even if the waiter resumes after the deadline. | http-surface §2.3 | `http::tests::reads::read_by_classifies_with_one_sample` |
| DL-CL-5 | When the future of `until_deadline` is first observed at or after its deadline, or its destructor runs past it, `until_deadline` shall answer 408. | http-surface §2.3 | `http::tests::reads::{until_deadline_drop_past_the_deadline_is_408, late_body_is_408}` |
| DL-CL-6 | Clippy shall reject a direct call to each of `tokio::time::{timeout, timeout_at, sleep, sleep_until}` in a target of `crates/daemon` that carries no `allow`. | D2 | CI step "Probe the timer ban" (`scripts/lint-probe.sh`); `cli/tests/lints.rs::clippy_tomls_list_the_bans` |
| DL-CL-7 | The kept sites shall keep their code, except that each tokio timer call becomes a call of its exception function at the same point with the same instant or duration (`listener::accept_backoff` holds its constant 1 s inside). | D3 | review of the diff; their existing tests (D4) |

## 5. Out of Scope

- The synchronous helpers of `pty-runtime::host`, the CLI's polls (`cli::poll`: `poll_until`, and `poll_until_with`, which reads its cutoff on an injected clock), ticket and pair-code expiry, and the store's blocking waits.
- A write's ticket: `run_write` and `write_with` await an admitted job's ticket to the writer's answer with no timeout of their own; `not_after` bounds only the writer's start of the job.
- The supervisor's await of `Session::spawn`, which has no deadline: cancelling it cannot stop the blocking call from creating the child, so the launch op awaits it to its result, bounded by the spawn lock's holders, each of which does bounded work; the request replies at its own deadline through the runner (`docs/design/supervisor.md` §2.10).
- hyper's `header_read_timeout`, `Sleep::reset`, `interval` (review-only).
- Moving the kept sites onto `settle_before`.
- The CLI crate's `clippy.toml`: the CLI has no tokio outside its tests.

## 6. Follow-ups

Open mismatches and deferred work:

- **F1.** Record removal: the retry pause is relative and computed before its first poll, so a pause between the two can carry the wait past the 500 ms budget, against daemon-lifecycle §2.8 step 5 ("the wait before a retry never runs past the budget"). No deletion follows the budget (it is checked before each deletion). Fix: an absolute wake instant.
- **F3.** WebSocket idle: the idle timer arm is polled before `stream.next()`, so a ready idle timer wins a tie, but a frame returned after the idle instant before the timer is seen ready renews `idle_at`. http-surface §2.6 says a socket "with no inbound frame for `idle_timeout`" is dropped. Decide whether such a frame may renew.
- **F4.** `ConnIo` write stall is progress first: ready I/O after a pause clears a due stall timer. Decide whether expiry must win.
- **F5.** `until_deadline` builds the handler's future before its first poll, so the guard's admission and the construction are separate steps. Moving construction into an admitted poll would make them one step; decide whether it should.
- **F6.** Tests pinning current behaviour of the kept sites: the hand-over serving a socket queued before the bound (the producer acknowledging its `tx.send`, then paused time past the recorded bound), a participant first polled after an already expired bound, an admitted WebSocket send crossing its deadline, the pid wait's post-wake sample on a matching commit with the logical cutoff passed and the timer pending, and no clock reading inside the reply wake.
