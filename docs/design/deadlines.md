# Design: One Deadline Rule for Async Waits

Status: design. Scope: the daemon's async waits that end at a deadline. The synchronous side already has its rule (`docs/design/daemon-lifecycle.md` §2.10 "Deadlines of start and stop", the four helpers of `pty-runtime::host`); this document gives the async side the same rule, one helper that carries it, and a lint that sends every new deadline wait through that helper.

This is its own document rather than a section of `http-surface.md` because the rule spans three designs (HTTP, hook ingest, lifecycle) and the lint covers the whole daemon crate.

## 1. Purpose

Reviews of the HTTP, lifecycle, and hook PRs kept finding the same defect in new shapes: a completion racing its deadline. `tokio::time::timeout_at` polls the inner future before its timer, so a result that became ready after the deadline is still delivered; a `select!` against a `sleep_until` has the same race unless something samples the clock after the result. Each fix was local (a clock check after `timeout_at` in `http/ctx.rs`, `before` in `http/ws.rs`, `deadline_first` in `lifecycle/shutdown.rs`), so the daemon now has three hand-written rules and nothing stops a fourth.

After this PR:

- One function, `deadline::settle_before`, decides every awaited result against a deadline, and its return type makes each caller say whether a late result counts.
- `crates/daemon/clippy.toml` forbids `tokio::time::timeout`, `timeout_at`, and `sleep_until`, so a new deadline wait written with the raw API fails CI unless it carries a reviewed `allow`.
- No public HTTP or CLI behaviour changes.

## 2. Survey

Command: `rg -n 'timeout_at|time::timeout|sleep_until|select!' crates --type rust`, non-test files only (paths without a `tests/` segment). 117 lines in all, **36 in non-test files**, all listed below. `crates/cli/src/daemon_cmd.rs` and `crates/pty-runtime/src/host/*` have no match: they are synchronous and already decide through the §2.10 helpers.

Kinds: **(a)** a result or state claim whose acceptance is decided against a deadline; **(b)** a plain bound on a wait, where a late result is harmless or discarded; **(c)** other (import, comment, periodic tick, backoff, shutdown signal, std sleep).

| Site | Kind | Current ordering | Correct today | Contract |
|---|---|---|---|---|
| `daemon/src/hook/mod.rs:317`, `:341` | a | Pid wait: `biased` timer first, then the commit receiver; after a notification, one sample `passed(pid_deadline)`; re-reads through strict `read_by` | yes | hook-ingest §2.3 "Pid wait" |
| `daemon/src/http/ws.rs:27` | c | import of `sleep_until` | n/a | — |
| `daemon/src/http/ws.rs:400` | c | doc comment of `before` | n/a | — |
| `daemon/src/http/ws.rs:404` | a | `before`: timer, then tokio clock sample, then poll; no sample after `Ready` | yes | http-surface §2.6 "Bounded disconnect" (clock sampled before each poll of a send) |
| `daemon/src/http/ws.rs:475`, `:480` | a | Startup read: `biased` shutdown, timer, join; the join's result classified by `take_read`'s one sample | yes | http-surface §2.3 "one cutoff", §2.6 |
| `daemon/src/http/ws.rs:536`, `:539`, `:540` | a | Socket loop: `biased` send and read timers drop the socket; a read result goes through `take_read`; a send through `before` | yes | http-surface §2.6 |
| `daemon/src/http/ws.rs:541` | b | Idle timer arm; an inbound frame that wins a tie with a passed idle deadline resets it | yes (a late frame only keeps a live socket open) | http-surface §2.6, mvp OQ4 timers |
| `daemon/src/http/ws.rs:543` | c | `heads_interval` tick | n/a | http-surface §2.6 |
| `daemon/src/http/ctx.rs:14` | c | import of `timeout_at` | n/a | — |
| `daemon/src/http/ctx.rs:71` | a | `read_cut`: `timeout_at`, then one sample `passed(deadline)` classifies the result | yes | http-surface §2.3 "one cutoff" (H16) |
| `daemon/src/http/ctx.rs:146` | a | `until_deadline`: `timeout_at`, then `passed(deadline)` | yes | http-surface §2.3 (read mounts, `body_within`); hook-ingest walk |
| `daemon/src/http/ctx.rs:294` | a | `await_write_releasing`: `timeout_at` to `reply_by`, then `passed(reply_by)` → `outcome_unknown` | yes | http-surface §2.3 plain writes |
| `daemon/src/http/listener.rs:44`, `:52`, `:68`, `:116` | c | Shutdown signal against permit, accept, the 1 s accept backoff, the connection | n/a | http-surface §2.2 |
| `daemon/src/http/commands.rs:31` | c | import of `timeout_at` | n/a | — |
| `daemon/src/http/commands.rs:568` | a | `await_reply`: progress read, then one sample; `timeout_at(cutoff, rx.changed())` only wakes the loop | yes | http-surface §2.8 |
| `daemon/src/http/commands.rs:751` | c | Completion backoff; the budget is decided by the clock read at the loop's top | n/a | http-surface §2.8 completion |
| `daemon/src/http/registry.rs:238`, `:240` | b | `wait_empty_until`: unbiased `select!`; `ServeHandle::shutdown` discards the bool and aborts what remains | yes | http-surface §2.2 "Shutdown" |
| `daemon/src/http/upgrade.rs:185` | b | Hand-over: `timeout(hand_over, rx)`, a socket ready after the bound is still served | yes (no contract refuses a late socket) | http-surface §2.4 hand-over bound |
| `pty-runtime/src/fake.rs:336`, `:348`, `:388`, `:394`, `:400`, `:403` | c | FakePty's own std `sleep_until` (thread sleep) | n/a | pty-runtime FakePty |
| `daemon/src/lifecycle/shutdown.rs:115` | a | `deadline_first`: std clock sample, poll, timer; no sample after `Ready` | yes | daemon-lifecycle §2.8 ("a poll admitted before the cutoff can return `Ready`") |
| `daemon/src/lifecycle/shutdown.rs:204` | c | `guarded`: console end against the step's future | n/a | daemon-lifecycle §2.8 |
| `daemon/src/lifecycle/shutdown.rs:513` | a | First wait: `biased` join; a join taken then is refused by `join_ended()`'s sample | yes | daemon-lifecycle §2.8 |
| `daemon/src/lifecycle/shutdown.rs:611` | b | Participants: `timeout_at`, result discarded | yes in code; see D1 | daemon-lifecycle §2.8 |

Per line: **(a) 14, (b) 5, (c) 17**.

**Defects found.** No code site accepts a late claim today. One document defect: daemon-lifecycle §2.8 says both "the participants' wait is a plain `tokio::time::timeout_at`" and "every bound of the coordinator's steps (… the participants' …) is a bound on waiting under the same rule (`deadline_first`)". The code follows the first sentence; since the result is discarded, no behaviour depends on it. This PR moves the participants to the helper with the `deadline_first` semantics and corrects §2.8 to one sentence (D1).

**What the survey shows about the rule.** The three hand-written rules differ:

| Helper | Sample before poll | Sample after `Ready` | Clock |
|---|---|---|---|
| `ctx` (`timeout_at` + `passed`) | no | yes, one sample classifies | server clock (`MonotonicClock`, injectable) |
| `ws::before` | yes | no | tokio clock |
| `shutdown::deadline_first` | yes | no | std clock |

They agree on outcomes only because each caller's contract tolerates the difference: claims (`ctx`) need the sample after the result; actions (a WebSocket send hands bytes to the transport on every poll) need the check before each poll; lifecycle bounds accept "a poll admitted before the cutoff".

## 3. Decisions

### D1. The helper

```rust
// crates/daemon/src/deadline.rs
pub(crate) enum Settled<T> { InTime(T), Late(T), Elapsed }

impl<T> Settled<T> {
    /// A state claim or a result: counts only in time.
    pub(crate) fn in_time(self) -> Option<T>;
    /// An action or a plain bound: a poll admitted before the deadline counts.
    pub(crate) fn admitted(self) -> Option<T>;
}

pub(crate) async fn settle_before<F: Future>(
    clock: &dyn MonotonicClock,
    deadline: std::time::Instant,
    fut: F,
) -> Settled<F::Output>;

/// tokio's clock as a `MonotonicClock` (`now` = `tokio::time::Instant::now().into_std()`,
/// `real` = identity), for waits timed on tokio's clock (WebSocket timers, the hand-over).
pub(crate) struct TokioMonotonic;
```

**The rule.** On every wake, in this order:

1. Sample the clock. At or after `deadline`: `Elapsed`, and `fut` is not polled.
2. Poll `fut`. On `Ready(v)`, sample the clock once more: at or after `deadline` → `Late(v)`, else `InTime(v)`. That one sample classifies the result.
3. Otherwise poll the timer `sleep_until(clock.real(deadline))`, created on the first poll; ready → `Elapsed`.

This is §2.10's rule on the async side: step 1 is `act_before`'s check before the action and `observed_before`'s check before the observation; step 2 is "each return is classified with one clock sample". A late result is handed back in `Late(v)`, never dropped inside the helper, so a caller that owns a resource in it (a sink, a join result, a reply) disposes of it. On `Elapsed` the helper drops `fut`, as `timeout_at` does today.

**Each call site chooses one mapping**, visible in review:

| Site | Mapping | Why |
|---|---|---|
| `ctx::read_cut` | `match`: `InTime(j)` → `classify(false, strict, j)`, `Late(j)` → `classify(true, strict, j)`, `Elapsed` → 408 | keeps `Busy`'s 503 for a non-strict late result (§2.3) |
| `ctx::until_deadline`, `ctx::await_write_releasing` | `in_time()`; `None` → 408 / 503 `outcome_unknown` | claims (§2.3) |
| `commands::await_reply` | `InTime(Ok(()))` → loop; anything else → `at_reply_time(&rx.borrow())` | the wait only wakes the loop; the loop's progress read and sample decide (§2.8) |
| `ws::close` (both), the socket loop's send | `admitted()` | an action: §2.6 samples before each poll |
| `shutdown` stopping rewrite, startup join, store report, participants | `admitted()` | §2.8: a poll admitted before the cutoff can return `Ready` |
| `upgrade` hand-over | `admitted()`, deadline `tokio now + short_deadline` taken where the bound starts today | a plain bound; a socket handed over counts |

`ws::before` and `shutdown::deadline_first` are deleted; their callers use `settle_before` with `TokioMonotonic` and `SystemMonotonic` respectively, the clocks they read today.

**Behaviour.** Unchanged for every migrated site except two differences inside a scheduling pause, both within their contracts: `read_cut` with `strict = false` answers 408 instead of 503 `busy` when an exhausted pool's result is first polled after the cutoff (§2.3: "408 or 503 `busy` depending on which is observed first"), and the hand-over and the participants no longer poll their future once the bound has passed (the hand-over drops a socket first seen after the bound; §2.8 "nothing waits past it").

**Alternatives.**

| Option | Gains | Gives up | Reversible | Cost |
|---|---|---|---|---|
| **Chosen:** one function, three-way result, mapping per site | one rule; the late case is a typed value each site must handle; no behaviour change | a site can still pick the wrong mapping (`admitted` for a claim); review catches it, tests in §5 pin the claim sites | yes: the enum can split into two functions later | one module, ~10 call sites |
| Strict only (`Late` is always a timeout) | no mapping choice at all | changes §2.6 (a send whose last poll began in time drops the socket) and §2.8 (a startup joined in such a poll becomes a bare exit) | yes | contract edits in two designs |
| Two functions (`claim_before`, `act_before_async`) | the name carries the choice | the same choice, made once by name instead of by method; `read_cut` needs the three-way value anyway | yes | slightly more surface |
| Keep the three local rules, add only the lint | smallest diff | the lint then has three `allow`s and no single rule to point to; the next site copies whichever it finds | yes | low now, recurring later |
| Shared crate | usable outside the daemon | `crates/core` is Sans-IO (no tokio, no clock); `pty-runtime` and the CLI have no tokio in production; a crate for one function | yes (moving is mechanical) | new workspace member |

**Home.** `crates/daemon/src/deadline.rs`, daemon-local: the daemon is the only crate with async production code. It takes `MonotonicClock` from `crate::http`, where the trait stays (its public path does not change).

### D2. The lint

`crates/daemon/clippy.toml` gains:

```toml
{ path = "tokio::time::timeout", reason = "decide through deadline::settle_before" },
{ path = "tokio::time::timeout_at", reason = "decide through deadline::settle_before" },
{ path = "tokio::time::sleep_until", reason = "a wait to a deadline: deadline::settle_before, or an allow with the rule the site follows" },
```

- **Where.** In the daemon's own `clippy.toml`, not a workspace-root one: clippy reads the nearest `clippy.toml` above the crate's manifest and does not merge it with a parent's, so a root file is shadowed by the existing `crates/{core,cli,daemon}/clippy.toml`. Verified with a scratch crate (deleted): a parent's list applied to a child without its own file, and was ignored once the child had one.
- **Coverage.** The scratch crate also showed the lint firing on every shape the codebase uses or could use: `use tokio::time::timeout_at;` then a bare call, the full path, a module alias (`use tokio::time as t; t::timeout`), a crate re-export (`pub use tokio::time::timeout as x`), the function taken as a value (as in `deadline.map(sleep_until)`), inside `#[cfg(test)]` modules, and in integration tests under `--all-targets`. `#[allow(clippy::disallowed_methods, reason = …)]` on a function or a module suppressed it.
- **Why `sleep_until` too.** `timeout` and `timeout_at` are the only public constructors of `tokio::time::Timeout`; `sleep_until` is the other half of the recurring shape (`select!` against a deadline). Deadlines here are absolute instants by design, so `sleep_until` is how a deadline wait is spelled. `tokio::time::sleep` (relative: backoffs, the write stall) and `interval` stay allowed; see open question 1.
- **Production `allow`s**, each `#[allow(clippy::disallowed_methods, reason = "…")]` on the narrowest item or statement, the reason naming the rule the site follows: `settle_before` (the one definition); `hook::pid_wait`'s timer (biased timer, then one sample); the WebSocket startup `select!` and socket-loop `select!` (timers drop the socket; results go through `take_read` or `settle_before`); `commands::complete`'s backoff (the budget is read at the loop's top); `registry::wait_empty_until` (a plain bound, result discarded). Five, plus the helper. The repository writes `allow` with a reason (`routes::build_router`, `lifecycle::spawn_locked`), so this follows it.
- **Tests are allowed**, at module roots: `#[allow(…)]` on `mod tests;` in `http/mod.rs` and `hook/mod.rs`, and `#![allow(…)]` at the top of each daemon integration test file and support module that uses these functions (`hook_b8`, `http_caps`, `http_commands`, `http_contract`, `http_events`, `http_layers`, `http_matrix`, `http_server`, `http_ws`, `support/commands.rs`, `support/http.rs`), with the reason "tests bound their own waits; deadline claims are tested with paused time or gates". About 50 call sites in 19 files only bound a test's wait; a late result there is harmless. A test-bound helper was considered: it would rename those calls with no gain in what they check. The rule that matters is the oracle's: a test of a deadline claim uses tokio's paused clock, an injected clock, or the fault gates, never a `timeout` as its oracle.

### D3. Migration scope

Moves to `settle_before` (D1 table): `ctx.rs:71`, `:146`, `:294`; `commands.rs:568`; `ws.rs` callers of `before` (`:373`, `:385`, `:388`, `:545`); `shutdown.rs` callers of `deadline_first` (`:415`, `:552`, `:650`) and `:611`; `upgrade.rs:185`. `hook/mod.rs:243`, `:262` follow through `until_deadline`.

Stays, with an `allow` (D2): `hook/mod.rs:317`, `ws.rs:480`, `ws.rs:536`–`:543`, `commands.rs:751`, `registry.rs:240`. These are multi-arm `select!`s or backoffs; their decisions are already a clock sample after the result, and wrapping a `select!` in the helper would add a second rule.

Documents the PR updates: daemon-lifecycle §2.8 (one sentence for all coordinator bounds, naming `settle_before`); http-surface §2.3 and §2.6 where they name `timeout_at` or the per-poll sample (name `settle_before` and the mapping); `cli/tests/lints.rs`'s module comment if it gains the check below.

### D4. Test strategy

**The helper** (unit tests in `deadline.rs`, deterministic: an injected `MonotonicClock` the test sets, futures built with `poll_fn`, no real-time sleeps):

| Claim | Test | Mutation it must catch |
|---|---|---|
| ready in time → `InTime` | clock before the deadline; future ready on first poll | `<` swapped for `<=` in step 2 |
| ready during a poll that ends at the deadline → `Late(v)` | the future's poll sets the clock to exactly the deadline and returns `Ready` | step 2's sample removed; `>=` changed to `>` |
| past the deadline → `Elapsed`, future unpolled | clock at the deadline before the first poll; the future counts its polls | step 1 removed |
| woken again before the deadline → re-checked | future pending, woken, clock advanced past the deadline, then ready | step 1 run only on the first poll |
| timer wakes a pending wait | `#[tokio::test(start_paused = true)]`, `TokioMonotonic`, future never ready → `Elapsed` | timer not polled (the test's own bound fails it under paused time) |
| `in_time` / `admitted` | each variant mapped | the two swapped |

**Claim sites.** Existing tests keep their names and must pass unchanged: `http::tests::reads::{late_read_success_is_408, late_read_error_is_408, late_body_is_408, late_admission_read_keeps_the_ticket, read_by_cutoff_precedes_the_outcome}`, `http::tests::writes::body_after_the_cutoff_submits_nothing`, the `http::tests::commands` reply-time tests, `http::tests::ws::{loop_drops_a_send_ready_after_its_deadline, close_drops_a_send_ready_after_its_deadline, send_deadline_drops_without_a_close_frame, close_keeps_the_send_deadline}`, and `lifecycle::tests::helpers::late_startup_in_the_first_wait_is_a_bare_exit`. After migration those tests reach the `Elapsed` path (the clock passes the deadline before the waiter polls), so a site that mapped `Late` as in time would still pass them. Each claim site that maps `Late` therefore gets one test of the `Late` path: a `#[cfg(test)]` task-local hook in `settle_before`, run between `Ready` and step 2's sample, moves the server's clock past the deadline. Sites: `read_cut` (success → 408; non-strict `Busy` → 503), `until_deadline` (→ 408), `await_write_releasing` (→ 503 `outcome_unknown`). Mutation per site: `in_time()` replaced by `admitted()` (or the `Late` arm by the `InTime` arm). These sites are awaited in the test's own task, which is what a task-local reaches. `await_reply` needs none: its two arms give the same answer.

**Action and bound sites** (`admitted`) keep their existing tests; the contract allows either answer for a poll admitted before the deadline, so no test pins `Late` there.

**The lint.** CI already runs `cargo clippy --workspace --all-targets --locked -- -D warnings` on macOS and Windows; with the entries in place, a raw call without an `allow` fails it. `cli/tests/lints.rs` already parses each crate's `disallowed-methods`; a second test there asserts `daemon/clippy.toml` lists the three paths, so deleting an entry fails a test, not only review.

## 4. Purpose Layer and Plan Layer

### 4.1 Purpose Layer: Acceptance Criteria

| ID | Criterion (EARS) | Source | How checked |
|---|---|---|---|
| DL-P-1 | When an awaited result that claims a state becomes ready at or after its deadline, the daemon shall answer as if no result came (408, 503 `outcome_unknown`, `in_progress` per §2.8, or the drop of the socket). | http-surface §2.3, §2.6, §2.8; daemon-lifecycle §2.10 "Late results" | the claim-site tests of D4 |
| DL-P-2 | If a change adds a call to `tokio::time::timeout`, `timeout_at`, or `sleep_until` in `crates/daemon` without an `allow`, then CI shall fail. | §1 | CI clippy job; `cli/tests/lints.rs` |
| DL-P-3 | The daemon's HTTP status codes, reply bodies, WebSocket close and drop behaviour, and shutdown steps shall stay as their designs state. | http-surface, daemon-lifecycle, hook-ingest | the full test suite passes with no test edited except added `allow`s and new tests |

### 4.2 Plan Layer: Claims

| ID | Claim (EARS) | Source | Falsifying test |
|---|---|---|---|
| DL-CL-1 | When `settle_before` is woken at or after its deadline, it shall return `Elapsed` without polling the future. | D1 step 1 | unit `deadline::tests::past_deadline_is_elapsed_unpolled`, `…::rechecked_on_every_wake` |
| DL-CL-2 | When the future returns `Ready`, `settle_before` shall classify it by one clock sample taken after the poll: `Late(v)` at or after the deadline, `InTime(v)` before it. | D1 step 2; daemon-lifecycle §2.10 | unit `deadline::tests::ready_in_time`, `…::ready_at_the_deadline_is_late` |
| DL-CL-3 | While the future is pending, `settle_before` shall return `Elapsed` once the timer at `clock.real(deadline)` fires. | D1 step 3 | unit `deadline::tests::timer_wakes_a_pending_wait` (paused time) |
| DL-CL-4 | When a read's result is `Late`, `read_cut` shall answer 408, except reader-pool exhaustion without `strict`, which stays 503 `busy`. | http-surface §2.3 | `http::tests::reads::late_settled_read_is_408`, `…::late_settled_busy_is_503` (task-local hook) |
| DL-CL-5 | When the future of `until_deadline` is `Late`, it shall answer 408. | http-surface §2.3 | `http::tests::reads::late_settled_handler_is_408` |
| DL-CL-6 | When a plain write's reply is `Late`, `await_write` shall answer 503 `outcome_unknown`. | http-surface §2.3 | `http::tests::writes::late_settled_reply_is_outcome_unknown` |
| DL-CL-7 | The WebSocket sends, the coordinator's bounds, and the hand-over shall take an admitted result as before. | http-surface §2.6; daemon-lifecycle §2.8 | existing `http::tests::ws` and `lifecycle::tests` suites, unchanged |
| DL-CL-8 | `crates/daemon/clippy.toml` shall list `tokio::time::timeout`, `tokio::time::timeout_at`, and `tokio::time::sleep_until` in `disallowed-methods`. | D2 | `cli/tests/lints.rs::daemon_disallows_raw_deadline_waits` |

## 5. Out of Scope

- The synchronous helpers of `pty-runtime::host` and the CLI's `poll_until` (already §2.10's rule).
- hyper's `header_read_timeout` and `ConnIo`'s write stall (library timer and a relative `sleep`; both plain bounds).
- `select!` sites that stay (D3); restructuring them around the helper.
- The CLI crate's `clippy.toml`: the CLI has no tokio outside its tests.

## 6. Open Questions

1. Also forbid `tokio::time::sleep`? It closes the relative spelling of a deadline wait; it costs three more `allow`s (`listener.rs:71` accept backoff, `listener.rs:172` the write stall, `shutdown.rs:253` the record-removal retry). The design leaves it out because every deadline in the daemon is an absolute instant.
2. The `Late`-path tests need a `#[cfg(test)]` task-local hook inside `settle_before`. The alternative is a clock that moves on its Nth reading, which ties the tests to how many times each site reads the clock. Confirm the hook.
