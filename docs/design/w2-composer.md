# Design: Composer and Basic Operations (MVP W2)

`docs/mvp-design.md` §4 row W2 (mvp:438, :480, :616, :631): PR 19 (composer, commands), M1. Extends `docs/design/w1-pwa.md`.

## 1. Scope

**Adds** to `packages/web`: an IME-guarded `<textarea>` composer; `send`, `ready`, `input_resolve`, `input_cancel`, `spawn`, `stop`, `resume`; per-thread inputs; an attention banner; `raw_unknown` counts; Playwright IME tests.

**Leaves** the terminal view, the key bar, and approval answers (raw keys, `card_key`) to T2 (mvp:439, :645); `close`, `task_set`, `mailbox_send`, and `stop --assume-dead` to the CLI; deferred items stay deferred (mvp:446). No daemon endpoint is added (each exists, mvp:211); the only Rust change exports launch-command types (§6).

## 2. Contract

### 2.1 Composer

One `<form>` per selected thread: a visible `<label>`, `<textarea>`, `<button type="submit">` (`forms` §1–2; `ime-safe-enter-submit` "Accessibility"). Drafts live in memory per thread.

**Enter** (`keydown`, `key === "Enter"`):

1. Composing (`isComposing`, `keyCode === 229`, a flag held from `compositionstart` to `compositionend`, or `|timeStamp − compositionend.timeStamp| < 50 ms`) → return **without** `preventDefault` (AGENTS:23; `ime-safe-enter-submit` strategies 1–2: WebKit fires `compositionend` before the confirming keydown; Android Chrome reports 229).
2. Shift+Enter → default (newline).
3. Ctrl/Cmd+Enter → submit on every device (no IME conflict, same guide).
4. Plain Enter → `preventDefault` and `requestSubmit()` only when `(pointer: fine)` matches; on a coarse pointer (soft keyboard) it inserts a newline and the button submits. `enterkeyhint` follows (`send` / `enter`; `forms` §3).

Sizing: `field-sizing: content`, `width: 100%`, bounded height, fixed-size fallback (`form-fields-automatically-fit-contents`); `font-size: max(1rem, 1em)`, 48 px coarse-pointer targets (`forms` §5).

**Submit** sends the textarea value unchanged (LF, no trim or normalization; the adapter encodes, mvp:184). Empty text, or UTF-8 over 256 KiB (mvp:186), sends nothing. The draft clears only on `SendResult.state = queued`; on `rejected` it stays with the reason (supervisor-send:62, SB8 :291).

### 2.2 Commands

`POST /api/commands/<name>` with `{ command_id, args }` (mvp:211), W1's request policy (Bearer, Fetch `cors`, `redirect: "error"`, w1-pwa §2.2), a client timeout of 10 s (5 s commands) or 40 s (`spawn`, `resume`, `stop`: 30 s, supervisor.md:102-104).

| UI | Command | Args |
|---|---|---|
| Send | `send` | `{ thread_id, text, origin: "pwa" }` |
| Mark ready (confirm dialog stating the assertion, mvp:172) | `ready` | `{ thread_id, expected_launch_id }` |
| It arrived / Dismiss | `input_resolve` | `{ input_id, delivered: true / false }` |
| Cancel | `input_cancel` | `{ input_id }` |
| New thread (agent, absolute `cwd`, optional prompt and task) | `spawn` | `SpawnArgs` (mvp:216) |
| Stop (confirm dialog) | `stop` | `{ thread_id, expected_launch_id }` |
| Resume | `resume` | `{ thread_id, expected_launch_id }` |

`expected_launch_id` comes from the thread copy (§2.3). Commands need status `live` (w1-pwa:37) and, per thread, a valid copy; the textarea stays editable. Dialogs use `showModal()` (`accessibility` §12).

**`command_id`** is `crypto.randomUUID()`, generated once in the submit handler; the request body is frozen as bytes with it. **No command is retried automatically.** Outcomes (http-surface:563-610):

| Reply | Shown as | Next |
|---|---|---|
| 2xx, schema-valid | the result | done |
| other handler code (http-surface:565-578) | its message; `death_unconfirmed` names `chaldea stop <thread> --assume-dead` | done |
| 408; 503 `busy`, `shutting_down`, `disk_full`, `store_failed` | "not done" | Retry (same id) |
| 409 `in_progress`; 503 `outcome_unknown`; transport error; client timeout | "outcome unknown" | Retry (same id) or Forget |
| 409 `unknown_after_crash`, `conflict` | "check the thread" | done |
| 401 | W1's credential rule (w1-pwa §2.2) | done |

A retry replays or reports progress, never acts twice (http-surface:614). Each thread has at most one unresolved `send`; Send and Enter do nothing until it resolves or is forgotten (Forget keeps the draft and warns that sending again makes a new input). Results go to one `role="status"` region (`forms` §7, `accessibility` §8).

### 2.3 Thread Copy Additions

The thread copy (w1-pwa §2.3) gains, from its own stream only (F1):

- `launch`: id, `exited`, `death_confirmed`, from `ThreadSnapshot.launch` (`sync.rs:220`) and `launch_reserved`, `launch_exited`, `launch_death_confirmed`.
- `inputs` by id (state, reason, `created_by`, preview): `snapshot.inputs`, `input_created`, `input_state_changed`; 20 newest terminal kept.
- `rawUnknown`: `raw_unknown` events applied; exact for a copy replayed from seq 1, else "at least" (a snapshot counts its `recent_events`). The host copy counts unbound ones.

An epoch switch rebuilds them with the copy.

### 2.4 Inputs, Banner, Counts

| State | Shown | Actions |
|---|---|---|
| `queued` | waiting for readiness | Cancel; thread-level Mark ready |
| `sending`, `sent` | sending / waiting for the CLI to record it | none |
| `unconfirmed` | may have reached the CLI; never resent | It arrived, Dismiss |
| `acked`, `rejected`, `cancelled`, `dismissed` | final, with reason and `by` | none |

No state offers a resend (mvp:185).

**Attention banner** (`role="status"`): followed threads with causes, from `rowView` (F1), each linking to the thread. The selected thread's causes (mvp:141-147) carry actions: `input_unconfirmed` → resolve; `input_gate_timeout` → Cancel or Mark ready; `session_start_timeout`, `startup_stall` → Stop (the screen is T2's); `resume_blocked` → the `--assume-dead` hint. `awaiting_approval` is shown without action. Causes clear only through `status_changed` (mvp:149).

**Counts**: "N unknown events" (or "at least N") per thread and for the host.

## 3. Decisions

| # | Decision | Rejected | |
|---|---|---|---|
| D1 | No automatic command retry; Retry reuses the frozen id and body. | Auto-retrying `outcome_unknown` (receipt-safe, mvp:215, but hides an unknown outcome). | reversible |
| D2 | Enter submits on fine pointers only. | Everywhere (soft keyboards compose every word). | reversible |
| D3 | A composing Enter is left to the IME. | `preventDefault` first (the guide's form; it can interfere with the IME's own confirm key). | reversible |
| D4 | Commands in their own pure reducer; the sync reducer gains projections only. | One reducer (W1's fast-check suites would model commands). | reversible |
| D5 | `raw_unknown` counted client-side. | A snapshot field (schema and store change for a diagnostic). | reversible |
| D6 | `assume_dead`, `close`, approval answers stay out. | In the PWA (an owner assertion after reviewing evidence, mvp:216; answers are T2's). | reversible |
| D7 | Input state comes from the stream; replies name only the input and a rejection. | Optimistic state (a second source). | — |

## 4. Acceptance Criteria

**Purpose layer** (M1 owner checks, macOS, Claude Code).

| ID | Criterion (EARS) |
|---|---|
| W2-1 | While the owner composes Japanese with the macOS IME in Safari and Chrome, Enter confirming a conversion shall not send (P-11, mvp:573). |
| W2-2 | When the owner sends Japanese, emoji, decomposed marks, and newlines from the PWA, the prompt Claude records shall equal the expected form (P-6, mvp:568). |
| W2-3 | When the owner spawns, sends, marks ready, stops, and resumes from the PWA (390 px and desktop), each shall take effect and show its result. |

**Plan layer** (Playwright: Chromium, WebKit, desktop, 390 px, FakeCli as `claude`; each fails on revert).

| ID | Claim (EARS) | Test |
|---|---|---|
| W2-4 | The composer shall follow §2.1's Enter rules (CL-22, mvp:616). | Vitest `composer.keys`; Playwright `ime_sequences` |
| W2-5 | When an outcome is unknown, the page shall show it and send nothing until the owner retries with the same id and body; at most one `send` per thread is unresolved. | Vitest `commands.no_auto_retry` (fast-check), `commands.single_flight`; Playwright `send_outcome_unknown` |
| W2-6 | `input_created.text` shall equal the textarea value; the draft shall clear only on `queued`; oversize text shall send nothing. | Playwright `send_intact`, `send_rejected_keeps_draft`; Vitest `composer.limit` |
| W2-7 | Inputs shall render and offer actions per §2.4. | Vitest `inputs.project`, `inputs.actions`; Playwright `unconfirmed_resolve`, `gate_timeout_cancel` |
| W2-8 | The banner shall show each cause from its owning copy until its `status_changed`. | Vitest `attention.banner` |
| W2-9 | Counts shall be exact from seq 1 and "at least" over a snapshot. | Vitest `raw_unknown.count` |
| W2-10 | Operations shall map to §2.2's commands and args. | Playwright `spawn_stop_resume`, `ready_opens_gate`; Vitest `commands.map` |
| W2-11 | Outside `live`, no command shall be sent and drafts shall be kept. | Vitest `commands.locked`; Playwright `offline_locks` |
| W2-12 | The page shall request no `/screen`, attach, or `key`, and stay CSP-clean. | Playwright `request_policy`, `csp_clean` (extended) |

## 5. Risks

- **Double send**: one frozen id per submission, single flight, receipt deduplication (http-surface:614). Forget then Send is a deliberate second input.
- **Auto-resend**: none (D1); `unconfirmed` offers only resolve (AGENTS:19).
- **Lost Japanese text**: drafts survive every failure. Accepted: a reload loses them (in-memory state, mvp:220); nothing is sent.
- **IME gaps**: Playwright synthesizes compositions; real IMEs are W2-1; Android is unverified until Phase 2 (P-11).
- **Terminal text as state**: none is read (invariant 1).
- **Token scope**: the owner token issues only §2.2's commands; no raw key or attach write (AGENTS:27).
- **Ready misuse**: a false assertion may land a prompt in a dialog; it is attributed or `unconfirmed`, never resent (mvp:172, :185).
- **Re-pairing**: a retry under a new token answers `conflict` (http-surface:616).

## 6. Implementation Notes

*Outside the word limit; for the PR brief.*

**Files** (approximate added lines):

- `crates/core/tests/contracts_export.rs`: export TS and JSON Schema for `SpawnArgs`, `SpawnResult`, `ResumeArgs`, `ResumeResult`, `StopArgs` (`command.rs:105-190`; today only `StopResult`/`CloseResult` of the launch commands are exported, `contracts_export.rs:136`, `:176`), +15. Regenerated `packages/contracts/schema/*.json` and `src/generated/*.ts`.
- `packages/web/src/commands/`: `client.ts` (post, classify per §2.2's table, ~150), `reducer.ts` (pending commands, drafts, single flight, ~250), `commands.test.ts` (~300).
- `packages/web/src/sync/reducer.ts`, `state.ts`: §2.3 projections (~150); `inputs.test.ts`, `rawunknown.test.ts` (~250).
- `packages/web/src/ui/`: `Composer.tsx` (+ `keys.ts`, the pure Enter decision, ~200), `Inputs.tsx` (~120), `AttentionBanner.tsx` (~100), `ThreadActions.tsx` (stop, resume, ready dialogs, ~150), `SpawnForm.tsx` (~120), `App.tsx` and `ThreadList.tsx` wiring (~60); `composer.test.ts`, `ui.test.tsx` additions (~250).
- `packages/web/e2e/`: `composer.spec.ts`, `commands.spec.ts` (~450); `daemon.ts` gains `inputs(thread)` and per-test FakeCli mode (~40).

**`keys.ts`** decides from `{ key, shift, ctrlOrMeta, isComposing, keyCode, composingFlag, timeStamp, lastCompositionEnd, finePointer }` → `ime | newline | submit`; `composer.keys` is a table over every combination, with the 49/50 ms boundary and an inverted (negative) difference.

**`ime_sequences`**: Chromium through CDP `Input.imeSetComposition` / `Input.insertText` and a 229 keydown via `Input.dispatchKeyEvent`; WebKit through dispatched `CompositionEvent` and `KeyboardEvent` (WebKit order: `compositionend` before the confirming keydown, `keyCode` 229 defined on the event). Each sequence asserts zero `POST /api/commands/send` and then one after a real Enter. At 390 px with `hasTouch` and a coarse pointer emulated, plain Enter inserts a newline.

**FakeCli triggers**: `echo` emits a `UserPromptSubmit` per line, so a single-line Japanese and emoji prompt is acked (`send_intact`), and a two-line prompt yields no matching hook and goes `unconfirmed` after 30 s (`unconfirmed_resolve`, test timeout 120 s). No `ready` leaves an input `queued` 60 s for `input_gate_timeout` (`gate_timeout_cancel`). `send_rejected_keeps_draft` sends `!ls` (Claude `lossy_text`, c1-claude-adapter D16). `send_outcome_unknown` aborts the first command response with `page.route` after the request reached the daemon, retries, and asserts one `input_created`.

**Messages**: one table from `ErrorCode` to text, exhaustive by type (a new code fails `tsc`).

### Canonical-doc edits proposed

1. mvp §2.4 "Offline" (:220): "locks input" → "sends no command outside `live`; drafts stay editable".
2. mvp §4 W2 row (:438) or §6.2: name the `raw_unknown` count source (client-side, "at least" over a snapshot, D5); nothing canonical states it today.
3. mvp :480 and CL-22 (:616): state the composer rule as §2.1 (the 50 ms window, Enter on fine pointers only, a composing Enter not prevented); CL-22's "W2 Playwright" covers synthesized compositions, real IMEs are P-11.
4. w1-pwa §2.3 "Status": "W2 locks input" → the wording of item 1.
