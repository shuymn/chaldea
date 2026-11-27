# Design: The Attach WebSocket (MVP T1)

Row T1 of `docs/mvp-design.md` §4 (mvp:436): the daemon side of `GET /api/attach/{thread}` (PR 11, M1), for T2's terminal view. It meets mvp:475 and CL-19's attach half (mvp:613).

## 1. Scope

**Adds.** The attach handler and socket loop (`crates/daemon/src/attach/`), public frames in `crates/core/src/attach.rs` (`ts-rs`), a `SessionHandle` and a bounded close in pty-runtime, a launch lease and an attach `human_write` interval in the send path, and inbound limits by ticket purpose.

**Does not add.** A client, the `chaldea attach` CLI, P-9, or an SGR serializer (deferred, mvp:451-452, :462); no command, event kind, or route. No state reads attach output.

## 2. Wire Contract

Admission is PR 7's: an owner-only `attach:<thread>` ticket, single use, 30 s (http-surface §2.4).

| Dir | Frame | Content |
|---|---|---|
| ← | `attached` (first) | `thread_id`, `launch_id`, `cols`, `rows`, `cursor`, `modes { bracketed_paste, alt_screen, show_cursor, app_cursor, app_keypad, kitty_flags }`, `grid` (pty-runtime §2.6 encoding) or `null` |
| ← | binary | PTY output after the snapshot, gapless |
| ← | `size` | PTY size after a resize attempt, `ok` |
| ← | `paste_result` | `paste_id`, `outcome` (`written`, `incomplete`, `pending`, `not_written`, `rejected`), `bytes_written: u64 \| null` (null on `pending`), `reason` |
| ← | `input_dropped` | `bytes` not written (whole unit on `pending`), `reason` |
| ← | `ended` | `launch_id`, `reason` (`eof`, `error`, `gone`) |
| → | binary | deliberate input only (keys, IME commits, key bar); empty ignored |
| → | `resize` | `cols` 2–512, `rows` 1–256 |
| → | `paste` | `paste_id` (socket-local, never replayed), `text` |

**Reasons.** `rejected`: a `RejectReason`, `prompt_too_large`, `bracketed_paste_off`. Writer and session: `expired`, `bracketed_paste_off`, `queue_full`, `busy`, `incomplete`, otherwise `not_written`. Raw only: `paste_marker`, `too_large`, `pending`.

**Close codes.** 4000 `no_live_launch`, 4001 `launch_ended` (after `ended`), 4002 `lagged`, 4003 `replaced`, 4004 `busy`; 1001 shutdown, 1003 malformed frame or size, 1007 invalid UTF-8, 1008 token revoked, 1009 over the limit. Best effort.

**Limits.** Inbound message: 6 × `MAX_PROMPT_BYTES` + 4 KiB for attach (any JSON escape of a 256 KiB paste), 64 KiB for `/api/ws`; raw frame 64 KiB; paste text 256 KiB; `attached` ≤ 192 KiB whole; queued output 1 MiB; drain 5 s; send 10 s per frame; idle 75 s.

## 3. Decisions

| # | Decision | Rejected alternative |
|---|---|---|
| D1 | One socket serves one launch; process state comes from sync; a resume needs a new socket. | Following the thread across launches. |
| D2 | One operating socket per launch for M1; a newer one takes over (4003). T2 shows why and offers take-back, without auto-reconnect. | Several viewers with last-resize-wins. |
| D3 | Initial screen: the snapshot, then the redraw the first resize forces (a one-row nudge at equal size). T2 applies `modes` to its terminal, keeps operate off after `grid: null` or a failed resize until the human enables it, and offers Redraw. | A daemon VT serializer (deferred). |
| D4 | Clipboard text arrives only as `paste`; T2 intercepts paste and suppresses ghostty-web's query replies. | Recognising pastes in raw input: an unmarked paste would submit early on Codex for Windows. |
| D5 | A paste on a bracketed-paste row needs DECSET 2004 on at encoding and dispatch. | Unwrapped text: CR newlines submit each line. |
| D6 | One outstanding unit per socket; raw input joined; T2 holds submit keys until `paste_result`. | One unit per frame: lost middle units, stale Enter after a failed paste. |
| D7 | One launch lease orders resize against sends; resize runs outside the op lock. | A flag beside `running`; resize holding the op lock. |
| D8 | A lagging client gets the prefix, then 4002. | In-place resync. |
| D9 | Raw input is written as sent on every row; T2 notes that typed non-ASCII may be lost on Codex for Windows; X2 decides on keyed entry. | Keying raw input now. |
| D10 | Inbound limits by ticket purpose. | Chunked paste frames. |
| D11 | Attach records `human_write` once per interval (mvp:88); `key` and `card_key` stay per unit. | One event per attach unit. |
| D12 | The snapshot copy is budgeted; over budget `grid` is `null` with the same offset. | A larger write buffer. |

All are reversible except D4's public frame.

## 4. Invariants

| ID | Invariant | Test |
|---|---|---|
| I-1 | `Session::close` returns within one absolute deadline even while a resize holds the PTY control, reporting the control unreleased until it closes; a resize starts only before seal. | pty `close_with_resize_held`, `resize_after_seal_refused` |
| I-2 | Every unit and resize carries its socket's grant from takeover; replacement, revocation, disconnect, and launch end revoke it, so no unpermitted old write or resize reaches the PTY; permitted writes finish. | `old_enter_after_disconnect_and_reattach`, `delayed_old_submission`, `old_resize_parked_before_backend`, `failed_candidate_keeps_current` |
| I-3 | Only the lease's holder, by acquisition id, submits or resizes; an open whose outcome is unknown keeps it until exit; `stop` never waits for resize. | `resize_waits_for_opened_send`, `open_waits_for_resize`, `open_result_unrecorded`, `invalidation_races_conversion`, `stop_with_resize_held` |
| I-4 | Pending input stays ≤ 64 KiB plus one validated message; after `pending` nothing more is submitted until that unit settles. | `crossing_paste_kept`, `max_paste_with_raw_outstanding`, `stuck_writer_after_pending` |
| I-5 | `ready → key → attach` records `human_write { attach }`; `attach → card_key → attach` records it once; gate and ack decisions are unchanged. | `ready_key_attach_records`, `attach_card_key_attach_skips`, `typing_records_once`, `worker_exit_overlap` |
| I-6 | No complete bracketed-paste marker reaches the PTY from raw input; a lone Escape is written at once. | `marker_split_at_every_position`, `lone_escape_immediate`, `dsr_answered_once`; T2: paste with 2004 off yields only `paste`, a rendered DSR yields no client frame |
| I-7 | Revocation starts closing within one read deadline of the nudge (30 s plus one when lost) and cuts unpermitted writes. | `revoked_at_startup`, `revoked_read_parked_in_snapshot`, `revoked_lost_nudge_under_load`, `slot_with_cleanup_held` |
| I-8 | The first terminal event wins; prefix and terminal frame go within 5 s, else the socket drops without one. | `drain_cutoff_drops`, `lag_then_gone_keeps_4002`, `lag_stalled_releases_slot` |

## 5. Acceptance Criteria

FakePty on both runners (Codex is `win_keys` on Windows); daemon tests `attach::tests::…` through the production mount.

| ID | Criterion (EARS) | Test |
|---|---|---|
| T1-1 | If a non-owner asks for an attach ticket, 403; if another purpose or thread is redeemed, 401 unspent. | `owner_only`, `production_mount_admission` |
| T1-2 | When a socket attaches, `attached` equals the snapshot, then exactly the later bytes; at the 192 KiB cutoff the grid is kept, past it `null`. | `initial_screen_gapless`, `attached_at_cutoff`, `envelope_tips_over` |
| T1-3 | When the first resize arrives, the PTY resizes, nudging at an equal size; a failed leg reports `ok: false`. | `first_resize_forces_redraw`, `nudge_superseded`, `nudge_second_leg_fails` |
| T1-4 | When binary frames arrive, the bytes are written unchanged and in order, and the gate closes. | `raw_input_order`, `input_closes_gate` |
| T1-5 | When a paste is accepted, the encoder's `AttachPaste` text is written as one part, no submit. | `paste_one_part_without_submit` |
| T1-6 | If a paste is refused (empty, 256 KiB + 1, `check`, 2004 off or toggled, writer), zero bytes, no event, the mapped reason. | `paste_refusals`, `paste_2004_toggled`, `refusal_mapping`, `report_channel_lost` |
| T1-7 | When a thread resumes with the old socket open, a new socket attaches the new launch, untouched by the old one. | `resume_with_old_socket_held` |
| T1-8 | When output ends or the session goes, `ended` and 4001; no live launch 4000; busy 4004; shutdown 1001. | `ends_on_eof`, `ends_on_error_child_alive`, `ends_on_gone`, `no_live_launch`, `startup_busy`, `shutdown_closes_1001` |
| T1-9 | If a frame breaks a limit or the format, the stated reason or close; the TypeScript contracts include the attach types. | `frame_limits`, `worst_case_escape_fits`, `events_limit_unchanged`, `bad_frames`, `committed_contracts_equal_generated_output` |

## 6. Risks and Accepted Limits

- Attach never feeds state (invariant 1).
- Accepted: typed non-ASCII to Codex on Windows (D9); unpadded paste triggers (input-encoders D10); L-11's late `Stop` after a turn started through attach (supervisor-send L-11).
- Accepted: the redraw depends on the CLI honouring resize; a sequence split at the snapshot renders wrong until it redraws (D3).
- Accepted: after revocation, permitted writes finish.

## 7. Proposed Canonical-Doc Edits

1. mvp:88, :116 and `GateEvidence`: I-5 for attach; per unit for `key`/`card_key`.
2. mvp §2.3 (:176): the gate also waits for the lease.
3. mvp §2.4 (:205): "opens a `/api/ws` socket", both occurrences.
4. mvp §2.5 (:239): attach protocol in this document.
5. mvp §4 T1 (:475): F5 by I-3; T2 (:439, :480): D3, D4, D6, D9 duties and I-6's client evidence.
6. supervisor-send §2.3 (:101-107), §2.6 (:198), SB11 (:304): lease, attach interval, `NotStarted(Refusal)`.
7. pty-runtime §2.2, §2.7, §4-§5: `SessionHandle`, I-1, budgeted subscribe, one operating socket per launch.
8. http-surface §2.6 (:430, :462-472): limits by purpose; revocation rationale scoped to `/api/ws`.
9. http-surface §2.11 (:648), §4 (:738): `UpgradedSocket`/`UpgradeGrant` for `AttachGrant`.
10. http-surface :757, PR7-CL-26 (:826): binary and `subscribe` rules scoped to `/api/ws`.
11. input-encoders §2.3 (:178), §4 (:336): paste text as a JSON string.
12. architecture §5 (:77): T1 uses FakePty, X1 FakeCli; D9's limit.
