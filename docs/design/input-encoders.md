# Design: Input Encoders and the Adapter Capability Table (MVP PR 5)

Status: design. Scope is the PR 5 row of `docs/mvp-design.md` §4 (cited as "mvp §N"): the `core::input` encoders and the adapter capability table. The contract implemented is mvp §2.3 (what an encoder reports to the input state machine: rejection with zero bytes, text and submit as one unit), §2.7 ("Text encoding", "First prompt", "Submit", the `win_keys` paragraph), §8 D6, the PR 5 obligation of mvp §4, the PR 5 half of claim CL-19, and the unit tests of the arch §5 open item **Codex keyed input** (mvp §4 "Open items of arch §5"). It builds on the single writer of `docs/design/pty-runtime.md` (cited as "pty §N") and the contract types in `crates/core/src/{input,payload,sync}.rs`, none of which changes.

**What PR 5 decides and what it leaves.** PR 5 owns pure functions in `core`: input text and an adapter row in, bytes plus a submit (or a typed rejection) out, and the expected form of the prompt the CLI records. It owns the `(agent, os)` table rows those functions read: the PTY text encoding, the first-prompt delivery, and the submit delay. It does not decide when an input is checked or written, the gate, the ack attribution rules, or how a recognised attach paste is framed (PR 10, PR 11); which hooks, flags, readiness modes, or resume arguments a CLI gets (PR 10, 13, 14); or the wire shape of `GET /api/adapters` (PR 10). It names the functions those PRs call (§4).

**Commitments from earlier PRs this design keeps.**

1. No public type changes: `RejectReason`, `InputEncoding`, `InputCreated`, `InputInfo`, and `GateView` are used as they are (`crates/core/src/input.rs:71-85`, `:237-238`, `payload.rs:286-294`, `sync.rs:200`). `packages/contracts/` is unchanged; the contract drift test stays green without regeneration.
2. `core` stays Sans-IO (`crates/core/clippy.toml`, CI "Check core dependencies"): no clock, no thread, no environment. A delay is data the caller acts on; the host OS is an input, never a `cfg`.
3. One encoded unit is one `WriteUnit` of the single writer (pty §2.3): the text is one `Part::Bytes` (one write call on whole acceptance), the submit delay is one `Part::Pause`, the submit is one `Part::Bytes`. The writer's `max_unit_bytes` default (pty §5: `MAX_PROMPT_BYTES` × 36 + 64) stays above the bound of §2.5.

Sources read for this document:

| Source | Revision | Read for |
|---|---|---|
| This repository | `89f5c7a` | `AGENTS.md`; `docs/architecture.md` §5; `docs/mvp-design.md` §2.3, §2.7, §3, §4, §6, §8; `docs/prior-art.md` rows 4a–4e; `docs/spikes/pty-hooks-spike.md` §1 D (F-1a–F-1f, F-3d), §3, §4.4, §4.11; `docs/design/{pty-runtime,hook-ingest,http-surface}.md`; `crates/core/src/{lib,input,payload,sync,ids}.rs`, `crates/core/clippy.toml`, `crates/pty-runtime/src/limits.rs`, `Cargo.toml`, `.github/workflows/ci.yml` |
| Spike [`spikes/pty-hooks`](https://github.com/shuymn/chaldea/tree/03b00b8c0462c032f5f4a4f057feafa8e366d1b5/spikes/pty-hooks) | `03b00b8` | `src/paste.rs` (validator, `map_newlines`, `encode`, `escape_spans`, `compose_marks`, `encode_win_keys_with`, `unit_records`, `expected_win_keys`, `match_prompt`, `corpus`, `corpus_f`, tests), `src/marks.rs`, `testdata/claude-2.1.322/ups-*.prompt.txt`, `Cargo.toml` |
| `unicode-normalization` | crate 0.1.26 | `src/lib.rs:55-94` (`char::{canonical_combining_class, is_combining_mark}`, `UnicodeNormalization::nfc`), `src/lookups.rs:81-90` (`is_combining_mark` = `General_Category=Mark`), `src/tables.rs:18` (Unicode 18.0.0), `Cargo.toml` (one dependency, `tinyvec` with `alloc`) |
| `unicode-segmentation` | crate 1.13.3 | `src/lib.rs:100`, `:252` (`graphemes(true)`, extended clusters), `src/tables.rs:17` (Unicode 17.0.0), `Cargo.toml` (no dependencies) |
| `tinyvec` | crate 1.13.3 | `Cargo.toml` (every dependency optional; default features empty) |

## 1. Goal and Dependencies

**Goal.** For every row of mvp §2.7 and every prompt, `core` produces the exact bytes Chaldea writes to the CLI's PTY, as one unit the single writer can write without splitting an escape sequence, or a `RejectReason` with no bytes at all; and it states, as a function, the prompt the CLI is expected to record, so encoder and expected form are tested against each other as a reference model over a corpus (invariant 4). The table is chosen by `(agent, os)` passed in at run time, never by DECSET 2004 and never by `cfg` (spike F-1f).

**Files.**

```
crates/core/src/adapter.rs          # new: HostOs, Capabilities, CAPABILITIES, capabilities()
crates/core/src/input.rs            # unchanged body; adds `pub mod encode;`
crates/core/src/input/encode.rs     # new: check, encode, expected_prompt, prompt_matches, private helpers, unit tests
crates/core/src/lib.rs              # adds `pub mod adapter;`
crates/core/tests/input_encode.rs   # new: corpus, goldens, reference model, proptests
crates/core/tests/data/claude-2.1.322/ups-{ascii,decomposed_kana,decomposed_latin,size_4k_plus_1}.prompt.txt
                                    # copied verbatim from the spike's testdata at 03b00b8
crates/core/Cargo.toml              # two dependencies
Cargo.toml                          # two workspace pins
```

`src/input.rs` with `src/input/encode.rs` is Rust's non-`mod.rs` layout: no file moves, and every existing path (`chaldea_core::input::*`) stays.

**Dependencies** (exact pins, as the workspace requires; both are the spike's pins, `spikes/pty-hooks/Cargo.toml` at `03b00b8`, and are listed for `core` in mvp §3):

```toml
# Cargo.toml [workspace.dependencies]
# NFC and the canonical combining class / General_Category=Mark of the `win_keys`
# composition and Claude's expected form (docs/design/input-encoders.md §2.4).
unicode-normalization = "=0.1.26"
# Extended grapheme clusters for the `win_keys` composition.
unicode-segmentation = "=1.13.3"
```

`crates/core/Cargo.toml` adds both under `[dependencies]` with `.workspace = true`. The only new transitive crate is `tinyvec` 1.13.3 (all of its own dependencies optional and off). Neither crate does I/O; the CI denylist (`tokio|rusqlite|axum|hyper|tower|getrandom`) needs no change. The two crates carry different Unicode versions (normalization 18.0.0, segmentation 17.0.0): a mark assigned only in 18.0 is its own cluster to the segmenter, is then composed alone, and stays as it is; the reference model (§2.4) uses the same functions, so the difference cannot make encoder and expected form disagree.

## 2. Types and Functions

### 2.1 The Rules

1. **Decide on the whole input before any output.** `check` reads every scalar before `encode` builds anything; `encode` returns `Result<Encoded, Rejection>`, so a rejection carries no bytes by type.
2. **No silent change.** A character the encoding or the CLI would drop or reinterpret is rejected, never stripped or replaced (prior-art row 4b). The only transforms are the documented ones of §2.4, and each appears in the expected form.
3. **One text part.** All text bytes of one input, every escape sequence included, are one `Vec<u8>`; the submit is separate and only for `Purpose::Send`.
4. **The table is data.** `(Agent, HostOs)` selects a `Capabilities` row from a `const` array; nothing else selects an encoding.

### 2.2 The Capability Table: `core::adapter`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostOs { Macos, Windows }

/// One row of mvp §2.7, the columns PR 5 owns. PR 10, 13, 14 add their columns here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub agent: Agent,
    pub os: HostOs,
    /// How a prompt written to the PTY is encoded: `BracketedPaste` or `WinKeys`.
    pub pty_encoding: InputEncoding,
    /// How the first prompt of a spawned (or resumed with `--prompt`) launch travels:
    /// `Positional` (argv) or the PTY encoding after readiness.
    pub first_prompt: InputEncoding,
    /// Wait between the text part and the submit part of a send unit.
    pub submit_delay: Duration,
}

pub const CAPABILITIES: [Capabilities; 4];
#[must_use]
pub fn capabilities(agent: Agent, os: HostOs) -> &'static Capabilities;
```

| `agent` | `os` | `pty_encoding` | `first_prompt` | `submit_delay` | Source |
|---|---|---|---|---|---|
| `Claude` | `Macos` | `BracketedPaste` | `BracketedPaste` | 300 ms | mvp §2.7; spike A-6 |
| `Claude` | `Windows` | `BracketedPaste` | `BracketedPaste` | 300 ms | mvp §2.7; spike C-1 (`5feaeaeb`) |
| `Codex` | `Macos` | `BracketedPaste` | `Positional` | 300 ms | mvp §2.7, §8 D5 |
| `Codex` | `Windows` | `WinKeys` | `Positional` | 300 ms | mvp §2.7, §8 D5; spike F-1 |

- `capabilities` is a `match` over both enums with no wildcard, so a new `Agent` or `HostOs` variant fails to compile until it has a row. `CAPABILITIES` holds the same rows in this order for listing; a test checks the two agree.
- The host OS reaches the daemon as a value: PR 10 maps `std::env::consts::OS` (`"macos"`, `"windows"`) to `HostOs` once at startup and refuses to start on any other value. Tests call every row on every host.
- `pty_encoding` is never `Positional`, and `first_prompt` is `Positional` or equals `pty_encoding` (a table test checks both).
- `submit_delay` is 300 ms: the spike's tested value (`--enter-delay-ms`, spike §4.4) and herdr's production value (prior-art row 4e). It is internal (mvp §2.3 "Internal: window, timeout, and settle defaults") and below the writer's `max_pause` of 5 s (pty §5).
- `GateView::needs_bracketed_paste` (`input.rs:238`) is `pty_encoding == BracketedPaste` of the input's row; PR 10 fills it from the table.

### 2.3 The Encoder: `core::input::encode`

```rust
/// What the encoded text is for: `send` adds the submit; a recognised attach paste does not
/// (mvp §2.3, §2.7; CL-19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose { Send, AttachPaste }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    /// All text bytes: one `Part::Bytes`, one write call. Never empty.
    pub text: Vec<u8>,
    /// `Some` exactly for `Purpose::Send`.
    pub submit: Option<Submit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Submit {
    /// `Part::Pause` between the text and the submit; the row's `submit_delay`.
    pub delay: Duration,
    /// `Part::Bytes`; always `SUBMIT`.
    pub bytes: &'static [u8],
}

/// One CR: Return for both encodings (spike `paste.rs:389`, `:799`).
pub const SUBMIT: &[u8] = b"\r";

/// Why nothing was encoded. `reason` is what the input records (`InputInput::Rejected`);
/// `at` is diagnostic only (never persisted, never logged with the text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rejection {
    pub reason: RejectReason,
    /// Byte offset of the first offending scalar; `None` for `EmptyPrompt`.
    pub at: Option<usize>,
}

/// Encoded size bound: `text.len() + submit.bytes.len() ≤ MAX_EXPANSION × input.len() + MAX_OVERHEAD`.
pub const MAX_EXPANSION: usize = 36;
pub const MAX_OVERHEAD: usize = 15;

/// The rejections of §2.5 for `encoding` (any of the three, `Positional` included).
pub fn check(agent: Agent, encoding: InputEncoding, text: &str) -> Result<(), Rejection>;

/// `check(caps.agent, caps.pty_encoding, text)`, then the bytes of §2.6 / §2.7.
pub fn encode(caps: &Capabilities, text: &str, purpose: Purpose) -> Result<Encoded, Rejection>;

/// The prompt the CLI is expected to record for `text` (§2.4). Precondition: `check` is `Ok`.
pub fn expected_prompt(agent: Agent, encoding: InputEncoding, text: &str) -> String;

/// Whether a recorded `UserPromptSubmit.prompt` is the expected form of `text` (§2.4).
pub fn prompt_matches(agent: Agent, encoding: InputEncoding, text: &str, recorded: &str) -> bool;
```

- `encode` accepts any length; the prompt limit (`MAX_PROMPT_BYTES`, mvp §2.3) is admission's (PR 10, PR 11), before an input exists.
- `text` is `&str`: a caller holding bytes (attach) decodes UTF-8 first and treats invalid UTF-8 as its own refusal (PR 11).
- The Codex positional first prompt has no bytes to encode: PR 10 calls `check(agent, InputEncoding::Positional, text)` and passes `text` unchanged as the last argv element.
- `encode` with a row whose `pty_encoding` is `Positional` is unreachable (table test); it panics with a message rather than guess.

**Mapping to the writer** (PR 10's code, stated here because the escape-safety claim rests on it): `Send` → `WriteUnit { origin: Input { input_id }, parts: [Bytes(text), Pause(submit.delay), Bytes(submit.bytes)] }`; `AttachPaste` → `WriteUnit { origin: Attach, parts: [Bytes(text)] }`. Both shapes pass `validate` (1–3 parts, first and last `Bytes`, no empty `Bytes`, pause ≤ `max_pause`). The writer writes each `Bytes` part with one call on whole acceptance and counts a short accept in `split_writes` (pty §2.3), so no escape sequence of `text` is split by Chaldea; the pause runs inside the unit, after the gate recheck and with no write of any origin able to interleave (pty §2.3 "Writer loop", "Started units").

### 2.4 Expected Form

Building blocks (all on scalars of a valid `&str`):

- `lf(t)`: each CRLF, then each remaining CR, becomes LF.
- `nfc(t)`: whole-string NFC (`unicode_normalization::UnicodeNormalization::nfc`).
- `compose_marks(t)`: the spike's rule (`paste.rs:509-536`): for each extended grapheme cluster (`graphemes(true)`) holding a scalar with canonical combining class > 0 or `General_Category=Mark` (Mn, Mc, Me), NFC the cluster's scalars between CJK compatibility ideographs (U+F900–U+FAFF, U+2F800–U+2FA1F), which are kept as they are; every other cluster is unchanged.
- `tab4(t)`: each TAB becomes four spaces.
- `trim(t)`: `str::trim` (Unicode `White_Space`, so U+3000 and LF count).

| Agent, encoding | Logical text Chaldea delivers | Expected prompt | Evidence |
|---|---|---|---|
| Claude, `bracketed_paste` (both OSes) | `lf(t)` | `tab4(nfc(lf(t)))` | spike A-6 (2.1.322 macOS) and C-1 (2.1.326 Windows): TAB → 4 spaces, decomposed entries NFC-composed, every other entry exact, LF recorded (testdata `ups-size_4k_plus_1.prompt.txt`) |
| Codex, `bracketed_paste` (macOS) | `" " + lf(t) + " "` (§2.6) | `trim(lf(t))` | Codex source, tag `rust-v0.169.0` (spike §3): paste maps CR and CRLF to LF, no NFC, no tab expansion, `trim_submission` on submit |
| Codex, `win_keys` (Windows) | `" " + lf(compose_marks(t)) + " "` (§2.7) | `lf(compose_marks(t))` | spike F-1a oracle, `expected_win_keys` (`paste.rs:697-704`); `check` has already refused leading or trailing whitespace, so `trim` is the identity |
| Codex, `positional` | `t` (argv) | `t` | **unverified** (the spike's positional prompt was ASCII only); PR 14 fixtures settle it |

`prompt_matches` is `recorded == expected_prompt(…)`, except for Claude, where `recorded` also matches when it is exactly `"\n\n<pasted_content id=\"ID\">\n" + expected + "\n</pasted_content id=\"ID\">\n"` with one `ID` of `[A-Za-z0-9_-]+` used in both tags (Claude's large-paste wrapper, spike `strip_paste_wrapper`, `paste.rs:188-203`). The wrapper is accepted at any size because Claude's threshold is unknown (4 KiB + 1 got it, entries ≤ 1 KiB did not). Nothing else is normalized on the recorded side: a recorded CR, a decomposed form, or a differently expanded TAB is a mismatch, which leaves the input `unconfirmed`, never resent (mvp §2.3).

**Unverified parts of the Claude row**, each a fixture obligation of PR 13 and a P-6 corpus entry: whether Claude's NFC is whole-prompt (it turns U+FA19 into U+795E) or per cluster; whether Claude trims leading or trailing whitespace; the wrapper around a body holding TAB or marks. A wrong guess costs an `unconfirmed` input, not a wrong delivery.

### 2.5 Rejections

`check` applies these in this order and returns the first class that applies (precedence by class, not by position); `at` is the offset of that class's first offending scalar.

| # | Condition on `t` | `RejectReason` | Claude paste | Codex paste | `win_keys` | `positional` | Source |
|---|---|---|---|---|---|---|---|
| 1 | ESC, or a C0 control other than TAB, CR, LF | `ControlCharacter` | yes | yes | yes | yes | prior-art row 4b; mvp §2.3; spike `validate` |
| 2 | DEL (U+007F) or a C1 control (U+0080–U+009F) | `LossyText` | yes | yes | yes | yes | §3 F9 |
| 3 | `trim(t)` is empty (empty, or only `White_Space`) | `EmptyPrompt` | yes | yes | yes | yes | spike F-1d; mvp §8 D6 |
| 4 | `t` starts or ends with a `White_Space` scalar | `LeadingTrailingWhitespace` | — | — | yes | — | mvp §8 D6; spike F-1d |
| 5 | `t` contains TAB | `Tab` | — | — | yes | — | mvp §8 D6; spike F-1d |

- Rule 1 makes `ESC[201~` inside the text impossible, so no input can end a bracketed paste early or inject a key record.
- `SpawnFailed` is never produced here (it is the positional spawn's, mvp §2.3).
- No allow-list: the spike's `--allow-lossy trim|tab` is not lifted (mvp §8 D6: an explicit transform is additive later).

### 2.6 Bracketed Paste

`text = ESC[200~ + pad + nl_cr(t) + pad + ESC[201~`, where `nl_cr` maps CRLF, LF, and a lone CR each to one CR (prior-art row 4c, §3 F7), and `pad` is one space for Codex and empty for Claude (§3 F8). Every other scalar is its UTF-8 bytes. The submit is `SUBMIT` after `submit_delay`.

The only ESC bytes in `text` are the two markers (rule 1), so `text` holds exactly two escape sequences, both complete, at its two ends. `text.len() ≤ t.len() + 14`.

### 2.7 `win_keys`

After `check`: `s = " " + lf(compose_marks(t)) + " "`. Then each scalar `c` of `s`, in order, appends:

| `c` | Bytes | Evidence |
|---|---|---|
| U+0020–U+007E | its one byte | mvp §2.7 ("ASCII as UTF-8") |
| LF | Shift+Enter key-down and key-up: `ESC[13;28;13;1;16;1_ESC[13;28;13;0;16;1_` (Vk 13, Sc 28, Uc 13, Cs `SHIFT_PRESSED` 16, Rc 1) | spike `NewlineKey::W32` (`paste.rs:427`); arch §5 (`ESC[13;2u` written by the host loses the newline, F-1 `d97ee701`) |
| any other BMP scalar (U+0080–U+FFFF, not a surrogate) | `ESC[0;0;<u>;1;0;1_ESC[0;0;<u>;0;0;1_`, `<u>` = the scalar's value in decimal, no leading zeros (Vk, Sc, Cs 0, Kd 1 then 0, Rc 1) | spike `unit_records` (`paste.rs:595-597`); mvp §4 PR 5; arch §5 (U+2764 sent as UTF-8 was lost; records delivered key-downs at the console boundary, phase 3c `adbefa2d`) |
| astral scalar (U+10000–U+10FFFF) | its four UTF-8 bytes | arch §5 (astral samples arrived intact as UTF-8; per-unit records of a surrogate pair would reach Codex's crossterm fork as high-down, high-up, low-down, low-up, `parse.rs:46`, `:258`) |

The scalars `check` refused never reach this table: TAB, CR (already LF), C0, ESC, DEL, C1. All of it is one `text` part; the spike's one write per line and per newline key (`paste.rs:662-684`) is not kept (§3 F6). The submit is `SUBMIT` after `submit_delay`; the gate's "ready composer only" (mvp §2.7 "Submit") is PR 10's.

**Size bound.** Per scalar of `s`: ASCII 1 byte → 1; LF 1 → 36; U+0080–U+07FF 2 → ≤ 34; U+0800–U+FFFF 3 → ≤ 36; astral 4 → 4. So each input byte yields at most 36 output bytes, plus 2 for the pads. `compose_marks` can lengthen a cluster (a composition exclusion decomposes: U+0344 → U+0308 U+0301, U+FB2C → three scalars), and these stay inside the bound (32 and 34 bytes per input byte); the exhaustive test of PR5-CL-12 checks it for every scalar rather than relying on this list. With the submit, `MAX_EXPANSION` 36 and `MAX_OVERHEAD` 15 (bracketed paste: 12 marker bytes + 2 pads + 1 submit) cover both encodings, and the writer's default (pty §5, 36 × + 64) is above them.

## 3. Structure Options

| Fork | Option | Precedent | Verdict |
|---|---|---|---|
| **F1. Module layout** | A. Grow `input.rs` with the encoders and the table | — | Rejected: the table is read by gate, supervisor, adapters, and `/api/adapters`, not only by input code. |
| | B. `core::input::encode` (non-`mod.rs` submodule) + `core::adapter` | spike `paste.rs` (encoders) next to its `input_encoding` table | **Adopted.** mvp §3 places both in `core`; no existing path moves. |
| | C. A separate crate | — | Rejected: `core` already owns `InputEncoding` and `RejectReason`; a crate would split the contract from its rules. |
| **F2. Unicode dependencies** | A. `unicode-normalization =0.1.26` + `unicode-segmentation =1.13.3` | spike pins (`Cargo.toml` at `03b00b8`); mvp §3 | **Adopted.** The spike's composition tests ran on these exact versions; one small transitive crate. |
| | B. ICU4X (`icu_normalizer`, `icu_segmenter`) | — | Rejected: data providers and a much larger tree for the same two operations. |
| | C. Hand-written tables (the spike's `marks.rs`, Mn/Me/Cf, Unicode 16.0) | spike `src/marks.rs` | Rejected: records now cover every non-ASCII BMP scalar (F9), so no category table is needed; NFC by hand is not justified (prior-art rule). |
| **F3. Encoded unit shape** | A. A list of writes (`Vec<Vec<u8>>`) | spike `encode`, `encode_win_keys_with` | Rejected: the writer takes at most 8 parts (pty §2.3), a multi-line prompt has more lines. |
| | B. `Encoded { text, submit: Option<Submit { delay, bytes }> }` | pty §2.3 "Send path" (`[text, Pause, submit]`) | **Adopted.** Maps one-to-one onto `WriteUnit` parts. |
| | C. `core` returns a `WriteUnit` | — | Impossible: `pty-runtime` depends on `core` (mvp §3 dependency direction). |
| **F4. Who waits the submit delay** | A. The supervisor sleeps, then submits the CR as a second unit | spike `paste`, `type_keys` (`thread::sleep`, `paste.rs:388`, `:793`) | Rejected: a human write or a VT reply could land between the text and its submit (mvp §2.3 asks for one unit). |
| | B. The delay is data (`Submit::delay`) and the writer runs it as `Part::Pause` inside the unit | herdr keeps its delay inside one submission (`src/pty/actor.rs:155-163`, pty §3 "Reply placement") | **Adopted.** Already the writer's contract (pty §2.3); `core` stays clock-free. |
| **F5. Invalid input** | A. Strip or replace controls | Zed, Alacritty, xterm.js, libghostty, tmux (prior-art row 4b) | Rejected: changes the prompt silently (invariant 4). |
| | B. Reject with zero bytes | prior-art row 4b; spike `validate` | **Adopted.** |
| **F6. `win_keys` write granularity** | A. One write per line and one per newline key | spike `encode_win_keys_with` (each queued in one `WriteMsg::Data`) | Rejected: needs more parts than the writer allows; its only purpose, keeping each sequence whole, holds with one part too. |
| | B. One part for all text | — | **Adopted.** Every record lies inside the one write. Not yet run against real Codex: PR 15's P-7 runs it (risk in §7). |
| **F7. Bracketed paste newline** | A. `cr` | xterm.js, tmux (prior-art row 4c); spike default | **Adopted.** The spike's sweep settled the measured variable: Claude arrived exact in `cr`, `lf`, and `crlf` on macOS (A-6) and Windows (C-1, 30 cases), recording LF; Codex's paste path maps CR and CRLF to LF (source). `cr` is also what Enter sends. |
| | B. `lf` / C. `crlf` | Alacritty, Zed, libghostty (LF); herdr on Windows (CRLF) | Rejected: no case where they do better; one form keeps one golden set. |
| **F8. Codex `/`, `!`, `?`, `@` triggers in a bracketed paste (macOS)** | A. Send the text as is | mvp §2.7 row as written | Rejected: Codex's composer dispatches a slash command from untrimmed text starting with `/` (`slash_input.rs:89-123`), and `!` and `?` at the start and an `@` token at the end change handling (spike §3); the composer is shared by both OSes. |
| | B. Pad with one leading and one trailing space, as `win_keys` does | spike F-1c (`win_keys` padding, `paste.rs:629-634`) | **Adopted.** Codex trims both spaces on submit, so the expected form is unchanged (`trim(lf(t))`). Not yet run on macOS: P-8's corpus must include the trigger entries of corpus F (PR 23). |
| | C. Reject such prompts | — | Rejected: legitimate prompts ("/review の件") would be refused. |
| **F9. DEL and C1 controls** | A. Accept (spike `validate` allowed `0x7f` and C1) | spike `validate` | Rejected: Codex's paste path removes control characters other than LF and TAB (`sanitize_user_text`, spike §3), and its Windows key path drops a control `u_char` with no usable virtual key (`parse.rs:176-180`); DEL reaching conhost as a key is unverified. Any of these silently changes the prompt. |
| | B. Reject as `LossyText` for every encoding | — | **Adopted.** Uses an existing reason ("text the adapter's encoding cannot carry without loss"); Claude's handling is unverified, and accepting them later is a loosening. |
| | C. A new `RejectReason` variant | — | Rejected: a public enum change for a case with no known use. |
| **F10. `win_keys` record scope** | A. Records only for BMP Mn, Me, Cf | spike phase 3c `--unicode-records marks` | Rejected: U+2764 (So) sent as UTF-8 was lost (arch §5). |
| | B. Every non-ASCII BMP scalar | mvp §2.7 | **Adopted.** |
| | C. Every non-ASCII scalar, astral as surrogate-unit records | — | Rejected: the crossterm fork pairs surrogates wrongly from separate records (`parse.rs:46`, `:258`). |
| **F11. `win_keys` newline key** | A. `ESC[13;2u` (`csi-u`) | Codex `windows_key_sequence.rs` reassembly | Rejected: written by the host, every such newline was lost (spike F-1, `d97ee701`; arch §5). |
| | B. Shift+Enter key records (`w32`) | spike `NewlineKey::W32` | **Adopted.** |
| **F12. `win_keys` composition** | A. Whole-prompt NFC | — | Rejected: turns U+FA19 into U+795E. |
| | B. NFC per cluster holding a mark, CJK compatibility ideographs kept | spike `compose_marks` | **Adopted.** |
| | C. None | — | Rejected: Codex lost combining marks (`が` → `か`, spike C-1). |
| **F13. Capability table form** | A. A function over strings or `cfg` | spike `input_encoding(agent: &str, os: &str, …)` | Rejected: typos compile; `cfg` hides rows from tests on the other OS (F-1f). |
| | B. A `const` array keyed by `(Agent, HostOs)` and an exhaustive `match` | mvp §2.7 ("a const table in `crates/core`") | **Adopted.** |
| | C. A runtime configuration file | — | Rejected: capability changes come with fixtures and tests; no config surface in the MVP. |
| **F14. Public exposure of the table** | A. `ts-rs` / `schemars` derives on `Capabilities` now | — | Rejected: PR 10, 13, 14 add columns (hook injection, readiness modes, resume); freezing a wire type now fixes its shape before they are designed. |
| | B. Internal Rust type now; PR 10 defines the `GET /api/adapters` body from it | http-surface §2.2 route table (`ReadAdapters`, PR 10) | **Adopted.** PR 5's columns map to wire fields directly (`agent`, `os`, `pty_encoding`, `first_prompt`, `submit_delay_ms`). |
| **F15. Where the expected form lives** | A. In PR 10's ack code | — | Rejected: it would duplicate the encoder's transforms, and the encoder could not be tested against it. |
| | B. In `core::input::encode`, next to the encoder | spike `expected_win_keys`, `match_prompt` in `paste.rs` | **Adopted.** PR 10 calls `prompt_matches`; attribution stays PR 10's. |

## 4. Same-Class Inventory

Later PRs that call these functions or read the table, and the seam each uses.

| PR | Uses | Seam |
|---|---|---|
| 10 supervisor | `capabilities(agent, host_os)` at reservation; `check(agent, encoding, text)` for the input's rejection (`InputInput::Rejected(reason)` from `queued`, mvp §2.3); `encode(caps, text, Send)` at write time, mapped to `WriteUnit` as in §2.3; `Encoded::text` guarded with `Guard::bracketed_paste` when `pty_encoding` is `BracketedPaste`; `GateView::needs_bracketed_paste` from the row; `InputCreated::encoding` = `first_prompt` for a positional first prompt, else `pty_encoding`; `prompt_matches` inside ack attribution; `GET /api/adapters` built from `CAPABILITIES`; `check(…, Positional, …)` before `launch_reserved` for a Codex `--prompt`. | `core::adapter`, `core::input::encode` |
| 11 attach | A recognised paste: decode UTF-8, `encode(caps, text, AttachPaste)`, one `WriteUnit { Attach, [Bytes(text)] }`; a `Rejection` → the paste is dropped with zero bytes (mvp §4 PR 11). Outer-terminal framing and recognition are PR 11's. | `encode` with `Purpose::AttachPaste` |
| 13 Claude adapter | Bracketed paste send through PR 10; fixtures that settle §2.4's unverified Claude points (whole vs per-cluster NFC, trimming, wrapper with TAB or marks) and Claude's handling of a leading `/`, `!`, `#` in a pasted prompt; a change lands in `expected_prompt` / `prompt_matches` or `check` and the Claude rows. | Claude rows; §2.4, §2.5 |
| 14 Codex adapter | `win_keys` for send and attach paste on Windows; the positional prompt and its expected form (fixture); `-c tui.disable_paste_burst=true` (its own flag column); herdr's paste-boundary key as the fallback if burst-off falls short (mvp §4 PR 13 / 14), which would change §2.7 only. FakeCli attach paste tests on `windows-latest` (mvp "Why the first tests run in PR 15"). | Codex rows; §2.4, §2.7 |
| 15 real Windows host | P-7 (corpus F through `send`) and P-9 (attach paste) with real Codex. A failure changes only the encoder, the table, or the attach paste path (mvp §4). | `encode` output as written by PR 10 / 11 |
| 19 PWA input | Shows `rejected` with its `RejectReason` from `input_state_changed`; may read `GET /api/adapters` for hints. Never encodes. | Events; `/api/adapters` (PR 10) |
| 22 fuzz | Targets `encode` (every row × both purposes; invariants of PR5-CL-1, -3, -4, -11, -12: no panic, `Err` ⇔ `check` `Err`, one complete escape-sequence grammar per encoding, the size bound, the reference-model decode equal to `expected_prompt`), `prompt_matches` (no panic; `expected_prompt` always matches), and `check`. Seeds: the corpus of §6. | `core::input::encode` |
| 23 real hosts | P-6, P-8 with the corpus of §6 (P-8 including corpus F's trigger entries, F8). | — |

## 5. Purpose Layer and Plan Layer

### 5.1 Purpose Layer: Acceptance Criteria

| ID | Criterion (EARS) | Observable result / failure | How to check |
|---|---|---|---|
| PR5-P-1 | When a prompt from the corpus is encoded for any table row, the bytes shall equal the spike-verified sequences for that encoding (markers, CR, unit records, Shift+Enter records, padding). | Pass: byte-equal goldens. Fail: any differing byte. | `cargo test -p chaldea-core` on `macos-latest` and `windows-latest`. |
| PR5-P-2 | When an accepted prompt's bytes are decoded by a model of the CLI's documented input handling, the result shall equal the prompt's expected form. | Pass: equal for every corpus entry and generated input. Fail: a lost, added, or changed scalar. | Same; proptest. |
| PR5-P-3 | If a prompt holds a character the encoding or CLI would drop or reinterpret, or is empty, then the encoder shall return the reason and no bytes. | Pass: the reason of §2.5. Fail: `Ok`, another reason. | Same. |
| PR5-P-4 | When the real CLIs receive these bytes, each prompt shall arrive once and equal its expected form (mvp P-6, P-7, P-8, P-9). | Real hosts only. | PR 15, PR 23; not checked in PR 5. |

### 5.2 Plan Layer: Claims

Tests named `encode::…` are unit tests in `crates/core/src/input/encode.rs`; `adapter::…` in `crates/core/src/adapter.rs`; `input_encode::…` in `crates/core/tests/input_encode.rs`. Every test runs on both CI runners (no `cfg`).

| ID | Claim (EARS) | Source | Test that falsifies it | Mutation it kills |
|---|---|---|---|---|
| PR5-CL-1 | If `check` rejects a text, then `encode` shall return that `Rejection` and no `Encoded`; and when `check` accepts it, `encode` shall return `Ok` for both purposes. | mvp §4 PR 5 ("zero bytes on rejected input"); mvp §2.3 | proptest `input_encode::encode_err_iff_check_err` (arbitrary `String`s with forbidden scalars inserted at a random position, every row) | `encode` validating only the first line, or only before the first newline; `encode` skipping `check` for `AttachPaste` |
| PR5-CL-2 | If a text contains ESC or a C0 control other than TAB, CR, LF anywhere, then `check` shall return `ControlCharacter` for every encoding, `at` naming its first occurrence. | prior-art row 4b; mvp §2.3 | `encode::controls_rejected` (each of U+0000–U+001F except TAB, CR, LF, at start, middle, end; literal `ESC[201~` and `ESC[0;0;65;1;0;1_` in the text); proptest `input_encode::control_anywhere_rejected` | ESC omitted from the set; NUL accepted; scan stopping at the first LF |
| PR5-CL-3 | If a text contains DEL or a C1 control and no rule-1 character, then `check` shall return `LossyText`. | §3 F9 | `encode::del_and_c1_are_lossy` (U+007F, U+0080, U+009B, U+009F; precedence with ESC present → `ControlCharacter`) | DEL accepted; C1 sent as a record; precedence reversed |
| PR5-CL-4 | If `trim(text)` is empty, then `check` shall return `EmptyPrompt` for every encoding. | mvp §2.3; §8 D6; spike F-1d | `encode::empty_and_blank_rejected` (`""`, `" "`, `"\u{3000}"`, `"\n"`, `"\r\n"`, `" \t "` for each encoding) | Whitespace-only accepted for Claude or positional; `win_keys` reporting `LeadingTrailingWhitespace` for `" "` |
| PR5-CL-5 | If a `win_keys` text starts or ends with a `White_Space` scalar, then `check` shall return `LeadingTrailingWhitespace`; if it contains TAB, `Tab`; and the bracketed-paste and positional encodings shall accept both. | mvp §8 D6; spike F-1d | `encode::d6_rejections` (`"\u{3000}一行目\n二行目"`, `"一行目\n二行目\n"`, `"a "`, `"\u{a0}a"`, `"列1\t列2"`; the same texts `Ok` for Claude and Codex macOS paste and positional) | ASCII-only whitespace test (U+3000 accepted); trailing LF accepted; D6 applied to macOS |
| PR5-CL-6 | When a text is encoded for a `BracketedPaste` row, `text` shall be `ESC[200~`, the pad, the text with each CRLF, LF, and lone CR as one CR, the pad, `ESC[201~`, with no other ESC byte; the pad shall be one space for Codex and empty for Claude. | mvp §2.7 (newline `cr`); prior-art rows 4a, 4c; §3 F7, F8 | goldens `input_encode::bracketed_goldens` (`"a\nb\r\nc\rd"` → `ESC[200~a\rb\rc\rdESC[201~` for Claude; `"/review の件"` → `ESC[200~ /review の件 ESC[201~` for Codex macOS; spike corpus entries); proptest `input_encode::bracketed_shape` | LF kept; CRLF → two CRs; pad added for Claude or dropped for Codex; marker typo |
| PR5-CL-7 | When a text is encoded for a `WinKeys` row, every ASCII scalar U+0020–U+007E shall be its byte, every other BMP scalar a key-down and key-up record pair carrying its value, every astral scalar its UTF-8 bytes, and each newline one Shift+Enter record pair, after one leading and one trailing space. | mvp §2.7; mvp §4 PR 5; arch §5 (U+2764) | goldens `input_encode::win_keys_goldens` over corpus F (spike `paste.rs:833-879`): U+200D → `ESC[0;0;8205;1;0;1_ESC[0;0;8205;0;0;1_`, U+FE0F 65039, U+3099 12441, U+309A 12442 (spike `marks_become_unit_records`), U+2764 10084, U+FA19 64025, `日` 26085, `𠮷野家` (astral UTF-8 + records), multi-line LF / CRLF / blank line with `ESC[13;28;13;1;16;1_ESC[13;28;13;0;16;1_`, `" /review の件 "`; proptest `input_encode::win_keys_byte_classes` (no byte < 0x20 except ESC, no 0x7F, no 2- or 3-byte UTF-8 sequence, every ESC starts one of the two record shapes) | Records only for Mn/Me/Cf (U+2764 as UTF-8); astral as surrogate records; `csi-u` newline; leading zeros or hex in `<u>`; key-up omitted; pads dropped |
| PR5-CL-8 | When a `win_keys` text holds a cluster with a combining or extending scalar, that cluster shall be NFC-composed with CJK compatibility ideographs kept, and every other cluster shall be unchanged. | mvp §2.7; spike F-1a, `compose_marks_per_cluster` | goldens `encode::compose_marks_cases`: `か\u{3099}` → `が`, `e\u{301}` → `é`, U+09C7 U+09BE → U+09CB, U+FA19 unchanged, U+FA19 U+0301 unchanged, U+31F7 U+309A unchanged, `U+FA19 か\u{3099}` → `U+FA19 が`, U+1100 U+1161 (Hangul L V, no mark) unchanged | Whole-prompt NFC (U+FA19 → U+795E, U+1100 U+1161 → U+AC00); a class-only rule (U+09BE has class 0); no composition |
| PR5-CL-9 | `encode` shall return exactly one text part holding every escape sequence it emits whole: for bracketed paste the two markers, for `win_keys` only complete records. | mvp §4 PR 5 (spike F-3d); arch §5 conhost | proptest `input_encode::escapes_complete` (every ESC in `text` begins a sequence of the encoding's grammar that ends inside `text`; `text` never ends inside one); `encode::submit_is_not_in_text` | Submit CR appended to `text` (bracketed `…ESC[201~\r`); a marker in its own part; a truncated record at the end |
| PR5-CL-10 | When `purpose` is `Send`, `encode` shall return `submit = Some { delay: row.submit_delay, bytes: b"\r" }`; when it is `AttachPaste`, `None`; and `text` shall be equal for both purposes. | mvp CL-19 (second half); §2.3; §2.7 "Submit" | proptest `input_encode::submit_only_for_send` (every row, corpus and generated) | Submit added for attach; delay 0 or from a constant other than the row; a CRLF or a w32 Enter record as submit; attach text differing |
| PR5-CL-11 | For every accepted text and row, decoding `text` with the CLI model shall give `expected_prompt`, and `prompt_matches(…, expected_prompt(…))` shall be true. | invariant 4; arch §5; mvp §2.3 "Ack" | proptest `input_encode::model_decode_equals_expected` (generator weighted over ASCII, kana, kanji, U+3000, combining marks, ZWJ, VS15/16, keycaps, flags, astral, CJK compatibility ideographs, CR/LF/CRLF, TAB, NFC expansions U+0344 and U+FB2C); the corpus run `input_encode::corpus_model` | Any dropped, duplicated, or reordered scalar; `nl_cr` mapping a CRLF to two newlines; a record with the wrong unit; pad not trimmed in the model of Codex |
| PR5-CL-12 | For every text, `encode`'s output shall be at most `MAX_EXPANSION × text.len() + MAX_OVERHEAD` bytes, text and submit together. | pty §5, D8 ("PR 5 bounds the encoded size") | `input_encode::size_bound_every_scalar` (every scalar alone, between `a` and `b`, and followed by U+0301, for every row); proptest `input_encode::size_bound`; `input_encode::max_prompt_worst_case` (`MAX_PROMPT_BYTES` of `"a\n"` repeated, then `"a"`, for `win_keys`) | Records with a padded fixed-width `<u>`; a second submit; a newline record emitted twice |
| PR5-CL-13 | The encoding shall be chosen by `(agent, os)` alone: Claude on both OSes and Codex on macOS shall have `pty_encoding = BracketedPaste`, Codex on Windows `WinKeys`; Codex shall have `first_prompt = Positional` on both OSes and Claude `BracketedPaste`; every row's `submit_delay` shall be 300 ms. | mvp CL-19 (first half); spike F-1f; mvp §2.7; §8 D5; prior-art row 4e | `adapter::table_rows` (all four rows asserted on every host, so a `cfg` choice fails on the other runner); `adapter::table_and_match_agree` (`CAPABILITIES` equals `capabilities` for every pair); `adapter::row_invariants` (`pty_encoding ≠ Positional`; `first_prompt` is `Positional` or `pty_encoding`). No function of `core::adapter` or `encode` takes a mode or VT parameter (signature review). | `cfg!(windows)` in `capabilities`; Codex macOS → `WinKeys`; Claude → `Positional`; delay 0 or 100 ms |
| PR5-CL-14 | For Claude, `prompt_matches` shall accept the recorded prompt when it equals `tab4(nfc(lf(text)))` or that text in Claude's exact large-paste wrapper, and reject every other difference. | arch §5; spike A-6, C-1; `match_prompt` | `input_encode::claude_recorded_prompts` (the four spike testdata files: ascii → TAB expanded, both decomposed → NFC, 4 KiB + 1 → wrapper); `encode::claude_mismatches` (2-space TAB, NFD recorded for composed text, wrapper with mismatched ids, wrapper around a shorter body, recorded CR) | Wrapper ids not compared; NFC applied to the recorded side; TAB expansion width 2 |
| PR5-CL-15 | For Codex, `prompt_matches` shall accept only `expected_prompt` exactly: `trim(lf(text))` for bracketed paste, `lf(compose_marks(text))` for `win_keys`, `text` for positional. | spike §3 (Codex source), F-1a oracle; mvp §2.3 "Ack" | `encode::codex_expected_forms` (corpus F entries; a padded text `" a "` via positional accepted unchanged; a Codex prompt with a wrapper rejected) | Wrapper accepted for Codex; NFC applied to `win_keys`' expected form (U+1100 U+1161); trim missing on macOS |
| PR5-CL-16 | `core` shall build and pass its tests without tokio, filesystem, network, process, or clock access, with the two new dependencies. | mvp CL-24 | CI "Check core dependencies"; `cargo clippy -p chaldea-core` with `crates/core/clippy.toml`; `cargo tree -p chaldea-core -e normal` lists `unicode-normalization v0.1.26`, `unicode-segmentation v1.13.3`, `tinyvec` and nothing on the denylist | A `thread::sleep` for the submit delay in `core`; a dependency that pulls tokio |

Coverage of the obligations handed to PR 5:

| Obligation | Claims |
|---|---|
| mvp §4 PR 5: zero bytes on rejected input | PR5-CL-1–5 |
| mvp §4 PR 5: never splits an escape sequence (spike F-3d) | PR5-CL-9 (and §2.3's mapping, tested by PR 10) |
| mvp §4 PR 5: `win_keys` byte sequences and Shift+Enter records over corpus F | PR5-CL-7, -8 |
| mvp §4 PR 5: submit delay | PR5-CL-10, -13 |
| mvp §4 PR 5: encoding by `(agent, os)`, never by DECSET 2004 (CL-19 first half, F-1f) | PR5-CL-13 |
| CL-19 second half: an attach paste never carries a submit | PR5-CL-10 (encoder half; PR 11 tests the attach path) |
| §2.3: what the encoder reports (`RejectReason`, text and submit as one unit) | PR5-CL-1–5, -9, -10 |
| §2.7 "Text encoding", "First prompt", "Submit" | PR5-CL-6, -7, -10, -13 |
| §2.7 `win_keys` paragraph | PR5-CL-5, -7, -8, -9, -10 |
| §8 D6 | PR5-CL-4, -5 |
| arch §5 prompt delivery, Bracketed Paste vs keyed input, U+2764; AGENTS.md invariant 4 | PR5-CL-7, -11, -14, -15 |
| "Open items of arch §5": unit tests of Codex keyed input | PR5-CL-5, -7, -8, -11 |
| pty D8: PR 5 bounds the encoded size | PR5-CL-12 |
| CL-24 with new dependencies | PR5-CL-16 |

## 6. Tests and Completion Commands

**Corpus** (`crates/core/tests/input_encode.rs`, one `const` table with ids): the spike's A-6 corpus (`paste.rs:70-111`: ASCII with TAB, Japanese, half-width kana, astral, emoji, ZWJ family, flag, `❤️`, decomposed kana and Latin, multi-line LF and CRLF, 4 KiB + 1); corpus F (`paste.rs:833-879`, including the recorded entries, which here are rejection cases); and these additions: U+09C7 U+09BE; U+FA19 U+0301; U+1100 U+1161; U+0344; U+FB2C U+05B0; keycap `1\u{FE0F}\u{20E3}`; U+E0100 after a kanji (astral variation selector); U+1D165; each C0 control; ESC alone and `ESC[201~` inside text; DEL; U+0080, U+009B, U+009F; empty; `" "`, `"\u{3000}"`, `"\n"`; leading and trailing space, U+3000, LF; TAB in the middle; a lone CR at the end; mixed CR / LF / CRLF; `MAX_PROMPT_BYTES` of mixed BMP text. Each entry lists, per row, `Ok(golden bytes)` or `Err(reason)` and its expected prompt.

**Reference model** (test-only, in `input_encode.rs`, independent of the encoder's code): `model_paste(agent, bytes)` checks and strips the two markers, maps CR to LF, then applies Claude's `tab4 ∘ nfc` or Codex's `trim`; `model_keys(bytes)` reads UTF-8 and `ESC[Vk;Sc;Uc;Kd;Cs;Rc_` records, takes key-downs only, maps Vk 13 with `SHIFT_PRESSED` to LF and `Uc` to its scalar, refuses any other shape, then applies Codex's `trim`. `nfc`, `tab4`, `trim`, and `lf` are restated in the test from §2.4; `compose_marks` is not (its goldens, PR5-CL-8, pin it).

**Proptests.** `proptest` (already a dev-dependency) with the default case count; strategies are weighted unions of the character classes listed in PR5-CL-11 plus arbitrary `String`s. A counterexample is fixed in the code or reported; the generator is never narrowed to avoid it.

**Mutation check.** Before merging, each mutation in §5.2's last column is applied by hand, the named test is seen failing, and the change is reverted; the PR description lists the result per claim.

**Completion commands** (both CI runners run the same core tests; nothing is `cfg`-gated, so the Windows test-listing step needs no new sentinel):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked                       # macOS; Windows CI runs scripts/windows-tests.ps1 -Lane Cargo
cargo test -p chaldea-core --test input_encode --locked
deps="$(cargo tree -p chaldea-core -e normal --prefix none)"; ! grep -Eq '^(tokio|rusqlite|axum|hyper|tower|getrandom) ' <<<"$deps"
git diff --exit-code packages/contracts               # no generated contract changes
```

## 7. Decisions

**D1. DEL and C1 controls are rejected as `lossy_text` for every encoding** (§3 F9). Codex drops them on both paths; Claude's handling is unverified. Reusing the existing reason keeps `RejectReason` unchanged. Reversible: accepting them for an adapter later is a loosening of `check`.

**D2. Codex's bracketed paste is padded with one space on each side** (§3 F8), so a prompt starting with `/`, `!`, `?` or ending in an `@` token is submitted as text on macOS as on Windows. Codex trims both spaces, so the expected form and the ack rule do not change. Reversible: the padding is encoder bytes only. It needs P-8's corpus to include the trigger entries.

**D3. `win_keys` writes all text as one part** (§3 F6), not one write per line as in the spike's verified runs. The writer's part limit leaves no other shape for a multi-line prompt, and escape safety needs only that no sequence crosses a write. PR 15's P-7 is the first real run of this shape; if Codex mishandles a single long write, the fallback is the writer-side change of allowing more `Bytes` parts per unit (pty §2.3), not a change of these functions' outputs.

**D4. The capability table's wire shape is PR 10's** (§3 F14). PR 5's Rust type is internal; its columns are listed for PR 10.

**D5. Whitespace-only text is `empty_prompt` for every encoding**, not only Codex on Windows: no CLI is known to submit it, and an input that cannot be acknowledged would otherwise sit `unconfirmed`. Reversible.

### Open Questions

None that change a public contract or are irreversible. Fixture obligations handed on: Claude's NFC scope, trimming, wrapper body, and its handling of a leading `/`, `!`, `#` in a pasted prompt (PR 13); the positional prompt's expected form (PR 14); real delivery of every-BMP-scalar records, one-part writes, and padding (PR 15, PR 23).

## Related Text in Other Documents

- `docs/mvp-design.md` §2.3 lists the `rejected` causes as "ESC or C0 other than TAB/CR/LF … Codex-on-Windows lossy text or empty prompt": D1 adds DEL and C1 (`lossy_text`, every encoding) and D5 makes whitespace-only `empty_prompt` for every encoding.
- `docs/mvp-design.md` §2.7 "Text encoding" row: Codex's bracketed paste carries D2's padding; the `win_keys` paragraph's "every escape sequence in one write" holds as one text part (D3).
- `docs/mvp-design.md` §7 "`cargo-fuzz`" row lists no encoder target; PR 22's targets are in §4 above.
- `docs/design/pty-runtime.md` §5: `max_unit_bytes` (36 × + 64) stays above PR 5's bound (36 × + 15); `crates/pty-runtime/src/limits.rs` may take `MAX_EXPANSION` from `core` instead of its own `KEYED_EXPANSION`.
- `docs/prior-art.md` rows 4a and 4c state the settled decisions this design implements: a bracketed paste waits for DECSET 2004 with the input `queued` and zero bytes written, and the newline inside the brackets is `cr`.
