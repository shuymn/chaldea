# Design: Composer and Basic Operations (MVP W2)

mvp §4 row W2 (mvp:438, :480, :616, :631): PR 19, M1; extends `docs/design/w1-pwa.md`.

## 1. Scope

**Adds** to `packages/web`: an IME-guarded composer; `send`, `ready`, `input_resolve`, `input_cancel`, `spawn`, `stop`, `resume`; per-thread inputs; an attention banner; `raw_unknown` counts. **Daemon:** an additive `raw_unknown_count` in host and thread snapshots (D5); TypeScript and JSON Schema exports of `SpawnArgs`, `SpawnResult`, `ResumeArgs`, `ResumeResult`, `StopArgs`. No new endpoint (mvp:211).

**Leaves** the terminal view, key bar, and approval answers to T2 (mvp:439, :645); `close`, `task_set`, `mailbox_send`, `stop --assume-dead` to the CLI (mvp:446).

## 2. Contract

### 2.1 Composer

A `<form>` per selected thread: visible `<label>`, `<textarea>`, `<button type="submit">`; helper buttons `type="button"`; the submit handler calls `preventDefault()` first (CSP `form-action 'none'`, w1-pwa:14). Drafts are in memory per thread (`forms` §1–2, §7).

**Enter** keydown, in order:

1. Composing — `isComposing`, `keyCode === 229`, a flag held from `compositionstart` to `compositionend` (reset on `blur` and thread switch), or within 50 ms (absolute) of the last `compositionend` (`null` initially) → nothing, no `preventDefault` (AGENTS:23; `ime-safe-enter-submit` strategies 1–2).
2. `repeat` → nothing.
3. Shift+Enter → newline.
4. Ctrl/Cmd+Enter → submit.
5. Plain Enter → submit when `(pointer: fine)` matches, else newline.

`field-sizing: content`, `width: 100%` (`form-fields-automatically-fit-contents`); `font-size: max(1rem, 1em)`, 48 px coarse targets (`forms` §5).

**Submit** sends the value unchanged (mvp:184). Empty text or UTF-8 over 256 KiB (mvp:186) sends nothing and marks the field `aria-invalid` with a described error. On `queued` the draft clears only if it still equals the submitted text.

### 2.2 Commands

`POST /api/commands/<name>`, `{ command_id, args }` (mvp:211), W1's request policy and 64 KiB reply cap (w1-pwa §2.2); client timeout 10 s, or 40 s for `spawn`, `resume`, `stop` (supervisor.md:102-104). Standalone validators check replies.

| Control | Command, args | When |
|---|---|---|
| Send | `send { thread_id, text, origin: "pwa" }` | open, process `starting`/`running` |
| Mark ready | `ready { thread_id, expected_launch_id }` | open, `running` |
| It arrived / Dismiss | `input_resolve { input_id, delivered }` | input `unconfirmed` |
| Cancel | `input_cancel { input_id }` | input `queued` |
| New thread | `spawn { agent, cwd, prompt?, task? }` (empty optionals omitted; agents from `Agent`) | always |
| Stop | `stop { thread_id, expected_launch_id }` | open, launch not exited |
| Resume | `resume { thread_id, expected_launch_id }` | open, exited, death confirmed |

An exited launch without confirmed death shows the hint instead of Resume: review with `chaldea stop <thread> --assume-dead`, confirm with `--launch <id>` (mvp:216; a refused resume raises `resume_blocked`, supervisor.md:122). Ready and Stop confirm in a `showModal()` dialog (`accessibility` §12) pinning the launch; a launch change closes it.

**Dispatch** needs status `live` and a *current* target copy: valid, no forward read due or in flight, cursor at the announced head or past it. Each control slot (the spawn form; a thread's send; a thread's stop, resume, ready; an input's resolve or cancel) holds one logical command, reserved synchronously before dispatch, Retry included. `command_id` is `crypto.randomUUID()`; the body is frozen as bytes with it and the credential generation. **Nothing retries automatically.**

| Reply (http-surface:563-610) | Means | Controls |
|---|---|---|
| 2xx, valid | the result: `rejected` with reason; `spawn_failed`; `death_confirmed: false` with the hint | — |
| 2xx, invalid | done, unreadable | — |
| other handler code; 409 `conflict`, `unknown_after_crash` (distinct texts) | settled | — |
| 409 `in_progress`; 503 `outcome_unknown`, unparsed 503; 500; transport; timeout | unknown | Retry after `retry_after`, Forget |
| 408; 503 `busy`, `shutting_down`, `disk_full`, `store_failed`; guard and parser 4xx; 501 | not done | Retry |
| 401 | not done; W1's compare-and-delete with the frozen generation | — |

Only a 2xx or settled reply settles: "not done" after an unknown leaves it unknown. The stream also settles an input command once its input leaves the acted state, and a stop or resume once the launch changes. An epoch switch, thread 404, or generation change marks unresolved commands unknown without Retry (receipts are per database and token, http-surface:616). Unresolved commands keep their controls in a thread-level area across disconnects. Forget keeps the draft, warning that a new send is a new input. Announcements use W1's `role="status"` region (StatusBar.tsx:54).

### 2.3 Thread Copy Additions

From the thread's own stream (F1):

- `launch`: id, `exited` (`exit_reason` set), `death_confirmed`; from `ThreadSnapshot.launch` (sync.rs:220) and `launch_reserved`, `launch_exited`, `launch_death_confirmed` of that id.
- `inputs`: state, `reason`, `by`, `created_by`, preview. Snapshots give non-terminal inputs without reason or `by` (omitted until a transition); 20 newest terminal by transition seq.
- `rawUnknown`: the snapshot's `raw_unknown_count` plus `raw_unknown` events applied after its base (replays count from 0). The host copy likewise.

### 2.4 Inputs, Banner, Counts

| State | Shown | Actions |
|---|---|---|
| `queued` | waiting for readiness | Cancel |
| `sending`, `sent` | in flight | — |
| `unconfirmed` | may have reached the CLI; never resent | It arrived, Dismiss |
| terminal | reason, `by` | — |

No resend anywhere (mvp:185); replies never roll back a streamed state (D7).

**Banner**: a `<section aria-label="Attention">`, not a live region, listing followed threads whose *valid* copy has causes (mvp:141-147) or `turn_state = awaiting_approval`; host "last known" values never appear there. Actions match the current input id and state: `input_unconfirmed` → resolve; `input_gate_timeout` → Cancel, Mark ready; `session_start_timeout`, `startup_stall` → Stop; `resume_blocked` → the hint; `awaiting_approval` → none (T2). Entries follow `status_changed`, snapshot replacement, and unfollowing.

**Counts**: exact, per thread and host.

## 3. Decisions

| # | Decision | Rejected | |
|---|---|---|---|
| D1 | No automatic retry; Retry reuses the frozen id and body. | Auto-retry (receipt-safe, mvp:215, but hides unknowns). | reversible |
| D2 | Plain Enter submits on a fine primary pointer (a preference, not keyboard detection). | Everywhere (soft keyboards compose words). | reversible |
| D3 | A composing Enter is left to the IME. | `preventDefault` first (can disturb confirmation). | reversible |
| D4 | Commands and drafts get their own pure reducer, reading W1's state. | One reducer. | reversible |
| D5 | Snapshots carry `raw_unknown_count`, counted in the snapshot's read transaction (`events_kind` index, schema.rs:92); no `schema_version` bump. | Client-side counting (snapshots hide earlier events). | reversible |
| D6 | `assume_dead`, `close`, approval answers stay out. | In the PWA (needs host evidence, mvp:216; T2). | reversible |
| D7 | Input state is the stream's. | Optimistic state. | — |

## 4. Acceptance Criteria

**Purpose layer** (M1 owner, macOS, Claude Code; record browser versions).

| ID | Criterion (EARS) |
|---|---|
| W2-1 | While the owner composes Japanese with the macOS IME in Safari and Chrome, a confirming Enter shall not send, and a later Enter shall (P-11). |
| W2-2 | When the owner sends Japanese, emoji, decomposed marks, and newlines, Claude's recorded prompt shall equal the expected form (P-6). |
| W2-3 | When the owner spawns, sends, marks ready, stops, and resumes (390 px, desktop), each shall take effect, witnessed in the thread, and show its result. |

**Plan layer** (Vitest; Playwright on Chromium and WebKit, desktop and 390 px, with fixtures where the operation matters; each fails on revert).

| ID | Claim (EARS) | Test |
|---|---|---|
| W2-4 | The composer shall follow §2.1's Enter rules, each guard alone. | `composer.keys`; `ime_sequences` |
| W2-5 | When an outcome is unknown, the page shall send nothing until Retry, which carries the same bytes; a later "not done" shall keep it unknown; each slot shall send one command per gesture. | `commands.retry`, `commands.slots`; `send_outcome_unknown`, `double_submit` |
| W2-6 | Stored input text shall equal the submitted value; drafts shall clear only per §2.1. | `send_intact`, `send_rejected_keeps_draft`, `draft_edit_during_send` |
| W2-7 | Inputs shall render and act per §2.4. | `inputs.project`; `unconfirmed_resolve`, `gate_timeout_cancel` |
| W2-8 | The banner shall show only valid-copy causes and turn state. | `attention.banner`; `gate_timeout_cancel` |
| W2-9 | Counts shall be exact across snapshots and replay. | daemon `snapshot_raw_unknown_count`; `raw_unknown.count` |
| W2-10 | Controls shall follow §2.2's availability, pinning, and replies. | `commands.map`; `ready_opens_gate`, `spawn_stop_resume`, `spawn_failed`, `stop_dialog_pins`, `command_401` |
| W2-11 | Without a current copy under `live`, no command shall be sent and drafts shall be kept. | `commands.locked`; `offline_locks`, `catch_up_after_hello` |
| W2-12 | The page shall send only §2.2's commands, never `/screen`, attach, or `key`, stay CSP-clean, and fit 390 px. | `request_policy`, `csp_clean`, `layout_390` |

## 5. Risks

- **Double send**: slots, frozen ids, receipts (http-surface:614). Forget then Send, or a reload after admission, is the owner's second input; W2-5 covers the current page.
- **Auto-resend**: none (D1; AGENTS:19).
- **Lost Japanese text**: drafts survive failures and late replies; reloads lose them (accepted, mvp:220).
- **IME gaps**: CI synthesizes compositions; real IMEs are W2-1; Android waits for Phase 2.
- **Token scope**: the owner token permits keys and attach (mvp:257); the page issues neither (W2-12).
- **Ready misuse**: a false assertion's prompt is attributed or `unconfirmed`, never resent (mvp:172).
