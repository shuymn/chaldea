# C1: Claude Code Adapter (lite) and the Rescue Read Surface

Design for mvp §4 row C1 (PR 12's Claude fixture checks and PR 13): a real Claude Code launch binds, takes prompts, opens and closes approval cards, records assistant text, and resumes; an external agent reads any thread through `GET /api/threads/{thread_id}/screen` and `chaldea inspect`. *New* marks identifiers absent at main; each PR's implementation detail (bounds, ordering, test procedures) lives in its implementation brief.

## 1. Acceptance criteria

FC = FakeCli/FakePty test in CI on macOS and Windows; M1 = dogfooding with real Claude on macOS (Windows at M2).

| ID | Criterion (EARS) | Check |
|---|---|---|
| C1-1 | When a Claude launch is reserved, its argv shall carry `--settings <state_dir>/claude-settings.json` and `--session-id` or `--resume`, and the user's settings files shall stay byte-equal. | FC; M1 P-1 |
| C1-2 | When Claude starts in a trusted folder, its `SessionStart` shall bind the launch and, with intake facts, record `session_start` readiness. | FC; M1 hook evidence |
| C1-3 | Every committed fixture shall decode to its recorded fact and events. | FC |
| C1-4 | If a decoder panics on a hook payload or transcript line, then the daemon shall keep the bytes as `raw_unknown` and keep serving. | FC (production panic hook) |
| C1-5 | When a corpus prompt is sent, Claude shall record it once in its captured expected form and the input shall end `acked`. | FC; M1 P-6 |
| C1-6 | If an input is not acknowledged in the ack window, then it shall end `unconfirmed` with no second submit. | FC; M1 P-15 |
| C1-7 | When a card is open, it shall resolve `ran` only under D15 and otherwise close at the turn boundary, and a card stale at open shall refuse its key with `card_closed`. | FC |
| C1-8 | While a launch is bound, the tailer shall store assistant text with its cursor atomically and start a new generation whenever continuity is disproved. | FC |
| C1-9 | If the transcript path is not an absolute regular file, then the tailer shall read nothing and block nothing. | FC |
| C1-10 | When a launch exits, the tailer shall read the rest (`partial_line` for a trailing fragment); when resumed, the same session shall bind and the cursor continue. | FC; M1 P-18 |
| C1-11 | When a reader of a thread requests its screen, the daemon shall answer `ScreenResponse` within its size bound and append no event. | FC |
| C1-12 | When `chaldea inspect` runs, it shall print the thread's state and screen, or the summary when the snapshot cannot be read. | FC; M1 external-agent entry |
| C1-13 | While Claude stalls before `SessionStart`, the thread shall carry `session_start_timeout` and the daemon shall write no byte. | FC |
| C1-14 | If a bound launch receives another session's `SessionStart`, then it shall be `foreign_session` and the binding shall stay. | FC (fixtures) |

After T2 at M1: P-12 and P-16 through the key bar, and the external agent's intervention. P-11 is W2's.

## 2. Decisions

**Public** marks public contract.

| # | Decision | Rejected |
|---|---|---|
| D1 | **Public** route `GET /api/threads/{thread_id}/screen`: `READERS`, the thread read check, `ReadStream`; errors reuse 404, 409 `no_live_launch`, 503 `busy`, 500 `internal`. **Public** `ScreenResponse` (*new*): `thread_id`, `launch_id`, `cols`, `rows`, `cursor`, `modes`, one text line per row, `truncated`; serialized body ≤ 512 KiB. | Owner only (orchestrators could not diagnose children); the cell grid. |
| D2 | **Public** `chaldea inspect <thread> [--json]` (owner token): one snapshot read through the `cursor_ahead` fallback with a 16 MiB cap, then the screen; `ThreadSummary` fallback; **public** `InspectOutput` (*new*) with `ErrorBody` errors; exit 0 / 1 / 2 / 4. | A snapshot route; raising the client's global cap. |
| D3 | **Public** Claude argv gains `--settings`; one settings file per daemon start, written in a fallible startup step. | One file per launch (cleanup for identical content). |
| D4 | Register six Claude events: `SessionStart`, `UserPromptSubmit`, `Stop`, `PermissionRequest`, `PreToolUse`, `PostToolUse`. | All eight (no kind carries `SessionEnd`, `Notification`). |
| D5 | A launch keeps its first binding across `/clear` and in-session `/resume`. | Following the new session (rebinding, migration). |
| D6 | Claude keeps binding rule `SessionId`. | `Ancestry` (unverified on real hosts). |
| D7 | A caught decoder panic is `raw_unknown` `schema_mismatch`; only pure parsing runs in the recovered region. | A new reason `decoder_panic`. |
| D8 | Expected prompt forms are fixed from captures; `prompt_matches` narrows to them. | Keeping every transform subset. |
| D9 | No VT snapshot when a card opens. | Recording one (`interpret` is data only). |
| D10 | Continuity is a SHA-256 of the read prefix, verified in full at every open and by `file_id` and size each poll. | The last 4 KiB (misses prefix rewrites); full re-hash per poll (O(n²)). |
| D11 | The CLI version comes from a bounded `claude --version` per launch. | `null` (arch §2 wants the version). |
| D12 | `GET /api/adapters` gains no readiness column. | A field no client acts on. |
| D13 | `open_regular` lives in `pty-runtime`, Windows FFI in `windows::sys`. | `cfg` code in the daemon. |
| D14 | Values over 64 MiB are one blob in internal ordered parts under one `BlobRef`, served by `/api/blobs`; the producer spools them to a temporary file in fixed-size buffers, and the writer inserts the parts from that handle with the event, refs, and cursor in one transaction. | Truncation (a second exception to verbatim `raw_unknown`); halting the tailer. |
| D15 | A `PostToolUse` resolves a card `ran` only when received after the turn watermark and the card, with exactly one equal open card; a card received below the watermark is closed at open. No other card rule. | Counting the turn's calls. |

The PRs bring canonical text in line with code: C1a mvp §2.5 (screen route), §2.3 (`input resolve --outcome`, mode `none` vs `OwnerOnly`); C1b mvp §2.7, §2.10, hook-ingest row 13, supervisor-send §2.5, PR10b-CL-13, CL-23, L-13, arch §2; C1c1 event-store §2.4–§2.5; C1c2 mvp §2.1 cursor rules.

## 3. Risks and limits

| Risk | Detection | Limit, and why the purpose holds |
|---|---|---|
| `SessionStart` before the trust prompt is answered. | M1 P-12. | If seen, Claude rows switch to `OwnerOnly`; `input_gate_timeout` asks for `ready`. |
| Readiness missing at bind without intake facts. | `inspect`. | Gate closed; the owner runs `ready`. |
| `--resume` starts a new session id. | M1 P-18. | Nothing is written to an unbound launch; a new thread recovers. |
| Windows hook shell rejects the command. | M2. | Fix is the settings string; macOS unaffected. |
| After `/clear` the thread follows the old session (D5). | `foreign_session`. | A send lands `unconfirmed`, never resent. |
| A denied card stays open until the boundary; a later equal call without a card marks it `ran`. | M1 P-16. | Only the label is wrong; no byte is written. |
| **L-13 after C1.** Card order is receipt order, not firing order. A turn's `UserPromptSubmit` received after its `PermissionRequest` and committed first closes a legitimate card at open. A dialog answered through attach, followed by a late `PermissionRequest` first taken in above the watermark, leaves an open card for a dialog that ended, and its card key can reach the composer. | The screen. | The owner sees `card_closed`, or an open card the screen contradicts, and answers through attach (T1, T2). |
| An in-place rewrite of read bytes on an open, growing file. | Next open. | Claude only appends; truncation shows at the next poll. |
| A resume before the old tailer's final read. | Tailer log. | The next tailer reads the bytes from the cursor. |
| Lenient forms where captures do not cover a transform. | C1-5. | Two candidates ack none. |

## 4. PR split

1. **C1a: read surface** (D1, D2; C1-11, C1-12). Beside C1b.
2. **C1b: hooks** (D3–D9, D11, D15; C1-1 to C1-7, C1-13, C1-14, hook halves of C1-4 and C1-10). Settings first, then fixture capture, then the decoder.
3. **C1c1: large blobs** (D14 store side). Independent.
4. **C1c2: tailer** (D10, D13, D14 producer side; C1-8, C1-9, tailer halves of C1-4 and C1-10). After C1c1.

M1 runs after all four; P-18 end to end: kill mid-turn, `resume` refused, evidence, `--assume-dead --launch`, same conversation resumed.
