# Design: Composer and Basic Operations (MVP W2)

mvp §4 row W2 (mvp:438, :480, :616, :631): PR 19, M1; extends `docs/design/w1-pwa.md`.

## 1. Scope

**Adds** to `packages/web`: an IME-guarded composer, seven commands (§2.2), per-thread inputs, an attention banner, `raw_unknown` counts. **Daemon:** an additive `raw_unknown_count` in host and thread snapshots (D5); TypeScript and JSON Schema exports of `SpawnArgs`, `SpawnResult`, `ResumeArgs`, `ResumeResult`, `StopArgs`. No new endpoint (mvp:211).

**Leaves** the terminal view, key bar, approvals to T2 (mvp:439, :645); `close`, `task_set`, `mailbox_send`, `--assume-dead` to the CLI.

## 2. Contract

### 2.1 Composer

A `<form>` per selected thread: visible `<label>`, `<textarea>`, submit button, `type="button"` helpers; submit calls `preventDefault()` (CSP `form-action 'none'`, w1-pwa:14). Drafts live in memory per thread (`forms` §1–2).

**Enter** keydown, in order:

1. Composing — `isComposing`, `keyCode === 229`, a flag held from `compositionstart` to `compositionend`, or within 50 ms (absolute) of the last `compositionend` (flag and timestamp reset on `blur` and thread switch) → nothing, no `preventDefault` (AGENTS:23; `ime-safe-enter-submit` strategies 1–2).
2. Shift+Enter → native newline.
3. Ctrl/Cmd+Enter, or plain Enter when `(pointer: fine)` matches → `preventDefault`, then submit unless `repeat`. Plain Enter otherwise → newline.

Sizing: `field-sizing: content`, `font-size: max(1rem, 1em)`, 48 px coarse targets (`forms` §5).

**Submit** sends the value unchanged (mvp:184); empty text or UTF-8 over 256 KiB (mvp:186) sends nothing, with an `aria-invalid` described error. On `queued` the draft clears only if its revision is still the submitted one.

### 2.2 Commands

`POST /api/commands/<name>`, `{ command_id, args }` (mvp:211), W1's request policy, 64 KiB reply cap, standalone validators (w1-pwa §2.2); timeout 10 s, or 40 s for `spawn`, `resume`, `stop` (supervisor.md:102-104).

| Control | Command, args | When |
|---|---|---|
| Send | `send { thread_id, text, origin: "pwa" }` | open, process `starting`/`running` |
| Mark ready | `ready { thread_id, expected_launch_id }` | open, `running` |
| It arrived / Dismiss | `input_resolve { input_id, delivered }` | input `unconfirmed` |
| Cancel | `input_cancel { input_id }` | input `queued` |
| New thread | `spawn { agent, cwd, prompt?, task? }` (empties omitted) | always |
| Stop | `stop { thread_id, expected_launch_id }` | open, launch not exited |
| Resume | `resume { thread_id, expected_launch_id }` | open, exited, death confirmed |

Unconfirmed death shows *the hint* instead of Resume: review with `chaldea stop <thread> --assume-dead`, confirm with `--launch <id>` (mvp:216; a refused resume raises `resume_blocked`, supervisor.md:122). Ready and Stop confirm in a `showModal()` dialog (`accessibility` §12) pinning the launch; a launch change closes it. Ready's dialog states the assertion: the pinned CLI's normal composer is open and empty, no popup or search; queued prompts may proceed.

**Dispatch** needs `live`; send, ready, and input commands also need a *current* copy (W1's `syncedCheck` per stream; equal non-null socket and copy epochs); Stop and Resume use the latest valid copy and the pinned launch. Each slot (spawn; per thread: send, stop or resume, ready; per input) holds one in-flight or unknown command, reserved synchronously before dispatch; a not-done command frees its slot, and its Retry reserves it again (refused if a newer one holds it). `command_id` is `crypto.randomUUID()`, frozen with the body bytes and credential generation. **Nothing retries automatically.**

| Reply (http-surface:563-610) | Means | Controls |
|---|---|---|
| 2xx, valid | the result: `rejected` with reason; `spawn_failed`; `death_confirmed: false` with the hint | — |
| 2xx, invalid | done, unreadable | — |
| other handler code; guard and parser 4xx; 501; 409 `conflict`, `unknown_after_crash` (own texts) | settled | — |
| 409 `in_progress`; 503 `outcome_unknown`, unparsed 503; 500; transport; timeout | unknown | "check the thread", Forget |
| 408; 503 `busy`, `shutting_down`, `disk_full`, `store_failed` | not done | Retry, Forget |
| 401 | not done; W1's compare-and-delete with the frozen generation | — |

Ambiguous outcomes are never retried: the browser cannot prove a retry reaches the receipts that saw the attempt (http-surface:616). Retry, after `retry_after`, exists only for not-done attempts within their socket session (no reopen, `hello`, epoch or generation change since). Only a 2xx or settled reply settles; a late reply (its context changed) updates only its own result, never drafts, selection, or newer slots. An unresolved command shows its state (a Send, its frozen text); nothing infers which command took effect. Forgetting an unknown command keeps the draft and warns that resending is new. W1's status region announces outcomes and refusals.

### 2.3 Thread Copy Additions

From the thread's own stream (F1):

- `launch`: id, `exited` (`exit_reason` set), `death_confirmed`; from `ThreadSnapshot.launch` (sync.rs:220) and that id's launch events.
- `inputs`: state, `reason`, `by`, `created_by`, preview. Snapshots give non-terminal inputs without reason or `by` (omitted until a transition); up to 20 terminal transitions observed by this copy.
- `rawUnknown`: the snapshot's `raw_unknown_count` plus accepted forward envelopes of `kind` `raw_unknown` after its base (replays from 0). Host likewise.

### 2.4 Inputs, Banner, Counts

| State | Shown | Actions |
|---|---|---|
| `queued` | waiting for readiness | Cancel |
| `sending`, `sent` | in flight | — |
| `unconfirmed` | may have reached the CLI | It arrived, Dismiss |
| terminal | reason, `by` | — |

No resend (mvp:185); replies never roll back streamed state.

**Banner**: a `<section aria-label="Attention">` (not a live region) of followed threads whose *valid* copy has causes (mvp:141-147) or `awaiting_approval` turn state, never host "last known" values. Actions match the current input id and state: `input_unconfirmed` → resolve; `input_gate_timeout` → Cancel, Mark ready; `session_start_timeout`, `startup_stall` → Stop; `resume_blocked` → the hint; `awaiting_approval` → none (T2). Entries follow `status_changed`, snapshots, and unfollowing.

**Counts**: per thread and host.

## 3. Decisions

| # | Decision | Rejected | |
|---|---|---|---|
| D1 | Retry only not-done attempts, with frozen id and body. | Retrying ambiguous outcomes (needs daemon-checked receipts). | reversible |
| D2 | Plain Enter submits on a fine primary pointer (a preference, not keyboard detection). | Everywhere (soft keyboards compose words). | reversible |
| D3 | A composing Enter is left to the IME. | `preventDefault` first (disturbs confirmation). | reversible |
| D4 | Commands and drafts get their own pure reducer. | One reducer. | reversible |
| D5 | Snapshots carry `raw_unknown_count`, counted in their read transaction (event-store §2.7); no `schema_version` bump; daemon, CLI, and PWA ship as one binary. A newer CLI or tab fails visibly on an older daemon's snapshots (inspect: unreadable snapshot; tab: schema error), losing and writing nothing, until the restart or reload an upgrade needs. | Client-side counting (snapshots hide earlier events). | reversible |
| D6 | `assume_dead`, `close`, approvals stay out. | In the PWA (host evidence, mvp:216; T2). | reversible |
| D7 | Input state is the stream's. | Optimistic state. | — |

## 4. Acceptance Criteria

**Purpose layer** (M1 owner, macOS, Claude Code; record browser versions).

| ID | Criterion (EARS) |
|---|---|
| W2-1 | While composing Japanese with the macOS IME (Safari, Chrome), a confirming Enter shall not send; a later Enter shall (P-11). |
| W2-2 | When Japanese, emoji, decomposed marks, and newlines are sent, Claude's recorded prompt shall equal the expected form (P-6). |
| W2-3 | When the owner spawns, sends, marks ready, stops, and resumes (390 px, desktop), each shall take effect and show its result. |

**Plan layer** (Vitest; Playwright on Chromium, WebKit, desktop, 390 px, with fixtures where the operation matters; each fails on revert).

| ID | Claim (EARS) | Test |
|---|---|---|
| W2-4 | The composer shall follow §2.1's Enter rules, each guard alone. | `composer.keys`; `ime_sequences` |
| W2-5 | While a slot's outcome is unknown, it shall send nothing; not-done Retry shall repeat the bytes in-session; one command per gesture. | `commands.retry`, `commands.slots`; `send_outcome_unknown`, `send_not_done_retry`, `double_submit`, `retry_session` |
| W2-6 | Stored input text shall equal the submitted value; drafts shall clear only per §2.1. | `send_intact`, `send_rejected_keeps_draft`, `draft_edit_during_send`, `oversize_no_post` |
| W2-7 | Inputs shall render and act per §2.4. | `inputs.project`; `unconfirmed_resolve`, `gate_timeout_cancel` |
| W2-8 | The banner shall show valid-copy causes and turn state only. | `attention.banner`; `gate_timeout_cancel` |
| W2-9 | Counts shall be exact. | daemon `snapshot_raw_unknown_count`; `raw_unknown.count` |
| W2-10 | Controls shall follow §2.2's availability, pinning, and replies. | `commands.map`; `ready_opens_gate`, `spawn_stop_resume`, `spawn_failed`, `stop_dialog_pins`, `command_401` |
| W2-11 | Outside `live`, or without the current copy §2.2 requires, no command shall be sent; drafts shall be kept. | `commands.locked`; `offline_locks`, `catch_up_after_hello` |
| W2-12 | The page shall send only §2.2's commands (no `/screen`, attach, `key`), stay CSP-clean, and fit 390 px. | `request_policy`, `csp_clean`, `layout_390` |

## 5. Risks

- **Double send**: slots and frozen ids; Forget then Send, or reload, is a deliberate input.
- **Lost Japanese text**: drafts survive failures and late replies, not reloads (accepted, mvp:220).
- **IME gaps**: CI synthesizes compositions; real IMEs are W2-1; Android is Phase 2.
- **Token scope**: the owner token permits keys and attach (mvp:257); the page uses neither.
- **Ready misuse**: the prompt is attributed or `unconfirmed`, not resent (mvp:172).
