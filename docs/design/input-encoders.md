# Design: Input Encoders and the Adapter Capability Table (MVP PR 5)

Status: design. Scope is the PR 5 row of `docs/mvp-design.md` §4 (cited as "mvp §N"): the `core::input` encoders and the adapter capability table. The contract implemented is mvp §2.3 (what an encoder reports to the input state machine: rejection with zero bytes, text and submit as one unit), §2.7 ("Text encoding", "First prompt", "Submit", the `win_keys` paragraph), §8 D6, the PR 5 obligation of mvp §4, the PR 5 half of claim CL-19, and the unit tests of the arch §5 open item **Codex keyed input** (mvp §4 "Open items of arch §5"). It builds on the single writer of `docs/design/pty-runtime.md` (cited as "pty §N") and the contract types in `crates/core/src/{input,payload,sync}.rs`, none of which changes.

**What PR 5 decides and what it leaves.** PR 5 owns pure functions in `core`: input text, an adapter row, and a purpose (`send` or a recognised attach paste) in; bytes plus a submit, or a typed rejection, out; and the expected form of the prompt the CLI records. It owns the `(agent, os)` table rows those functions read: the PTY text encoding, the first-prompt delivery, and the submit delay. It does not decide when an input is checked or written, the gate, the ack attribution rules, or how a recognised attach paste is framed (PR 10, PR 11); which hooks, flags, readiness modes, or resume arguments a CLI gets (PR 10, 13, 14); or the wire shape of `GET /api/adapters` (PR 10). It names the functions those PRs call and the obligations they carry (§4).

**Commitments from earlier PRs this design keeps.**

1. No public type changes: `RejectReason`, `InputEncoding`, `InputCreated`, `InputInfo`, and `GateView` are used as they are (`crates/core/src/input.rs:71-85`, `:237-238`, `payload.rs:286-294`, `sync.rs:200`). `packages/contracts/` is unchanged; the contract drift test stays green without regeneration.
2. `core` stays Sans-IO (`crates/core/clippy.toml`, CI "Check core dependencies"): no clock, no thread, no environment. A delay is data the caller acts on; the host OS is an input, never a `cfg`.
3. One encoded unit is one `WriteUnit` of the single writer (pty §2.3): the text is one `Part::Bytes`, the submit delay one `Part::Pause`, the submit one `Part::Bytes`. The writer's `max_unit_bytes` default (pty §5: `MAX_PROMPT_BYTES` × 36 + 64) stays above the bound of §2.8.

Sources read for this document:

| Source | Revision | Read for |
|---|---|---|
| This repository | `89f5c7a` | `AGENTS.md`; `docs/architecture.md` §5; `docs/mvp-design.md` §2.3, §2.7, §3, §4, §6, §8; `docs/prior-art.md` rows 4a–4e; `docs/spikes/pty-hooks-spike.md` §1 D (F-1a–F-1f, F-3d), §3, §4.4, §4.11, §7 (R12); `docs/design/{pty-runtime,hook-ingest,http-surface}.md`; `crates/core/src/{lib,input,payload,sync,ids}.rs`, `crates/core/clippy.toml`, `crates/pty-runtime/src/limits.rs`, `Cargo.toml`, `.github/workflows/ci.yml` |
| Spike [`spikes/pty-hooks`](https://github.com/shuymn/chaldea/tree/03b00b8c0462c032f5f4a4f057feafa8e366d1b5/spikes/pty-hooks) | `03b00b8` | `src/paste.rs` (validator, `map_newlines`, `encode`, `escape_spans`, `compose_marks`, `encode_win_keys_with`, `unit_records`, `expected_win_keys`, `match_prompt`, `corpus`, `corpus_f`, tests), `src/marks.rs`, `testdata/claude-2.1.322/ups-*.prompt.txt`, `Cargo.toml` |
| Codex CLI (`openai/codex`) | tag `rust-v0.171.0` | `codex-rs/cli/src/main.rs` (`MultitoolCli` usage `codex [OPTIONS] [PROMPT]` `:117-130`; CR/CRLF → LF of the prompt on spawn `:2546-2548` and resume `:2787-2789`; `import_remains_an_interactive_prompt` `:3117-3122`), `tui/src/cli.rs:16-18` (`prompt: Option<String>` positional); `tui/src/bottom_pane/chat_composer/paste_input.rs:214-280` (`apply_paste`: CR/CRLF → LF, `sanitize_user_text`, large-paste placeholder, image-path attach, shell prefix at position 0 `:37-45`); `tui/src/bottom_pane/chat_composer.rs:489` (`LARGE_PASTE_CHAR_THRESHOLD` 1000), `:1327-1351` (`handle_paste_image_path`), `:3187-3235` (submission: `input_starts_with_space` from the unexpanded text, expansion, `trim`, slash validation); `tui/src/bottom_pane/chat_composer/slash_input.rs:74-127` (`validate_submission`, `bare_command`, `inline_command`); `tui/src/bottom_pane/prompt_args.rs:7-25` (`parse_slash_name`); `tui/src/clipboard_paste.rs:231-275` (`normalize_pasted_path`); `tui/src/history_cell/messages.rs:41-71` (`sanitize_user_text` removes ESC-led CSI and every `char::is_control` other than LF and TAB) |
| `unicode-normalization` | crate 0.1.26 | `src/lib.rs:55-94` (`char::{canonical_combining_class, is_combining_mark, compose, decompose_canonical}`, `UnicodeNormalization::nfc`), `src/lookups.rs:81-90` (`is_combining_mark` = `General_Category=Mark`), `src/tables.rs:18` (Unicode 18.0.0), `Cargo.toml` (one dependency, `tinyvec` with `alloc`) |
| `unicode-segmentation` | crate 1.13.3 | `src/lib.rs:100`, `:252` (`graphemes(true)`, extended clusters), `src/tables.rs:17` (Unicode 17.0.0), `Cargo.toml` (no dependencies) |
| `tinyvec` | crate 1.13.3 | `Cargo.toml` (every dependency optional; default features empty) |

## 1. Goal and Dependencies

**Goal.** For every row of mvp §2.7, every purpose, and every prompt, `core` produces the exact bytes Chaldea writes to the CLI's PTY, as one unit the single writer writes without splitting an escape sequence, or a `RejectReason` with no bytes at all; and it states, as a function, the prompt the CLI is expected to record, so encoder and expected form are tested against each other as a reference model over a corpus (invariant 4). The table is chosen by `(agent, os)` passed in at run time, never by DECSET 2004 and never by `cfg` (spike F-1f).

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
# NFC, canonical decomposition, and General_Category=Mark for the `win_keys` composition
# and Claude's expected form (docs/design/input-encoders.md §2.4).
unicode-normalization = "=0.1.26"
# Extended grapheme clusters for the `win_keys` composition.
unicode-segmentation = "=1.13.3"
```

`crates/core/Cargo.toml` adds both under `[dependencies]` with `.workspace = true`. The only new transitive crate is `tinyvec` 1.13.3 (all of its own dependencies optional and off). Neither crate does I/O; the CI denylist (`tokio|rusqlite|axum|hyper|tower|getrandom`) needs no change. The two crates carry different Unicode versions (normalization 18.0.0, segmentation 17.0.0): `compose_marks` applies 18.0's Mark and NFC data to 17.0's cluster boundaries. A mark assigned only in 18.0 is `Any` to the segmenter, so it starts its own cluster, though a following scalar that is `Extend` in 17.0 joins that cluster. The reference model (§2.4) uses the same functions, so the version split cannot make encoder and expected form disagree; it only decides which scalars a cluster composes with.

## 2. Types and Functions

### 2.1 The Rules

1. **Decide on the whole input before any output.** `check` reads every scalar before `encode` builds anything; `encode` returns `Result<Encoded, Rejection>`, so a rejection carries no bytes by type.
2. **No silent change.** A character the encoding or the CLI is shown to drop or reinterpret is rejected, never stripped or replaced (prior-art row 4b). The only transforms are the documented ones of §2.4, and each appears in the expected form.
3. **The purpose decides only what the purpose needs.** The submit, the Codex padding, and the whitespace-only rejection exist for a submission (`Send`); a recognised attach paste inserts into a composer the human is editing and gets only the encoding and the rejections that protect its content.
4. **One text part.** All text bytes of one input, every escape sequence included, are one `Vec<u8>`; the submit is separate and only for `Send`.
5. **Total functions.** Every public function is defined for every argument; none panics (PR 22 fuzzes them).
6. **The table is data.** `(Agent, HostOs)` selects a `Capabilities` row from a `const` array; nothing else selects an encoding, and no caller can build a row.

### 2.2 The Capability Table: `core::adapter`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostOs { Macos, Windows }

/// One row of mvp §2.7, the columns PR 5 owns. PR 10, 13, 14 add their columns here.
/// Rows exist only in `CAPABILITIES`; `#[non_exhaustive]` keeps other crates from building one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
- `pty_encoding` is never `Positional`, and `first_prompt` is `Positional` or equals `pty_encoding` (a table test checks both). Because rows cannot be built outside `core`, `encode` never sees a `Positional` row.
- `submit_delay` is 300 ms: the spike's tested value (`--enter-delay-ms`, spike §4.4) and herdr's production value (prior-art row 4e). It is internal (mvp §2.3 "Internal: window, timeout, and settle defaults") and below the writer's `max_pause` of 5 s (pty §5).
- `GateView::needs_bracketed_paste` (`input.rs:238`) is `pty_encoding == BracketedPaste` of the input's row; PR 10 fills it from the table.

### 2.3 The Encoder: `core::input::encode`

```rust
/// What the encoded text is for: `Send` submits it into an empty composer and adds the submit;
/// `AttachPaste` is a recognised human paste inserted where the human is editing (mvp §2.3, §2.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose { Send, AttachPaste }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    /// All text bytes: one `Part::Bytes`. Never empty.
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
    /// Byte offset of the first offending scalar (§2.5); `None` for `EmptyPrompt` and for an
    /// unsupported combination.
    pub at: Option<usize>,
}

/// Encoded size bound: `text.len() + submit.bytes.len() ≤ MAX_EXPANSION × input.len() + MAX_OVERHEAD`.
pub const MAX_EXPANSION: usize = 36;
pub const MAX_OVERHEAD: usize = 39;

/// The rejections of §2.5 for `(agent, encoding, purpose)`.
pub fn check(agent: Agent, encoding: InputEncoding, purpose: Purpose, text: &str) -> Result<(), Rejection>;

/// `check(caps.agent, caps.pty_encoding, purpose, text)`, then the bytes of §2.6 / §2.7.
pub fn encode(caps: &Capabilities, text: &str, purpose: Purpose) -> Result<Encoded, Rejection>;

/// The prompt the CLI is expected to record for a sent `text` (§2.4); `None` when
/// `check(agent, encoding, Purpose::Send, text)` fails or the combination is unsupported.
pub fn expected_prompt(agent: Agent, encoding: InputEncoding, text: &str) -> Option<String>;

/// Whether a recorded `UserPromptSubmit.prompt` acknowledges a sent `text` (§2.4).
/// `false` whenever `expected_prompt` is `None`.
pub fn prompt_matches(agent: Agent, encoding: InputEncoding, text: &str, recorded: &str) -> bool;
```

- **Supported combinations** are those of the table: `(Claude, BracketedPaste)`, `(Codex, BracketedPaste)`, `(Codex, WinKeys)`, and `(Codex, Positional)` with `Purpose::Send`. For any other `(agent, encoding, purpose)`, `check` returns `Rejection { reason: LossyText, at: None }` ("the adapter's encoding cannot carry it": no adapter carries text this way), `expected_prompt` returns `None`, and `prompt_matches` returns `false`. `encode` reaches only supported combinations because it takes a table row.
- `encode` accepts any length; the prompt limit (`MAX_PROMPT_BYTES`, mvp §2.3) is admission's (PR 10, PR 11), before an input exists.
- `text` is `&str`: a caller holding bytes (attach) decodes UTF-8 first and treats invalid UTF-8 as its own refusal (PR 11).
- The Codex positional first prompt has no bytes to encode: PR 10 calls `check(Codex, Positional, Send, text)` and passes `text` unchanged as one argv element, after a `--` (§2.4 "Positional argv form").

**Mapping to the writer** (PR 10's and PR 11's code, stated here because escape safety rests on it): `Send` → `WriteUnit { origin: Input { input_id }, parts: [Bytes(text), Pause(submit.delay), Bytes(submit.bytes)] }`; `AttachPaste` → `WriteUnit { origin: Attach, parts: [Bytes(text)] }`. These are two of the shapes the writer accepts (1–8 parts, first and last `Bytes`, no empty `Bytes`, total pause ≤ `max_pause`, pty §2.3). The writer issues one write call per `Bytes` part; if a call accepts less, it writes the rest in further calls and counts `split_writes`, and the backends guarantee whole acceptance (pty §2.1, §2.3 "Short writes"; the Windows backend writes a synchronous pipe). So an escape sequence of `text` is split by Chaldea only if a backend breaks that guarantee, which `split_writes` makes visible; conhost may still read a long write in pieces of its own (spike R12), which only P-7 on a real host observes (§5.2 PR5-CL-17). The pause runs inside the unit, after the gate recheck, with no write of any origin able to interleave (pty §2.3 "Writer loop", "Started units").

### 2.4 Expected Form

Building blocks (all on scalars of a valid `&str`):

- `lf(t)`: each CRLF, then each remaining CR, becomes LF.
- `nfc(t)`: whole-string NFC (`unicode_normalization::UnicodeNormalization::nfc`).
- `compose_marks(t)`: the spike's rule (`paste.rs:509-536`): for each extended grapheme cluster (`graphemes(true)`) holding a scalar with canonical combining class > 0 or `General_Category=Mark` (Mn, Mc, Me), NFC the cluster's scalars between CJK compatibility ideographs (U+F900–U+FAFF, U+2F800–U+2FA1F), which are kept as they are; every other cluster is unchanged.
- `tab4(t)`: each TAB becomes four spaces.
- `trim(t)`: `str::trim` (Unicode `White_Space`, so U+3000, U+0085, and LF count).
- `wrap(id, b)`: `"\n\n<pasted_content id=\"" + id + "\">\n" + b + "\n</pasted_content id=\"" + id + "\">\n"`, `id` matching `[A-Za-z0-9_-]+` (spike `strip_paste_wrapper`, `paste.rs:188-203`).

| Agent, encoding | What Chaldea delivers | `expected_prompt` | `prompt_matches` accepts | Evidence |
|---|---|---|---|---|
| Claude, `bracketed_paste` (both OSes) | one paste of `lf(t)` | `tab4(nfc(lf(t)))` | `T(lf(t))` or `wrap(id, T(lf(t)))` for each `T` of `{id, tab4, nfc, tab4∘nfc}` | spike A-6 (2.1.322 macOS: TAB → 4 spaces, decomposed entries composed, 4 KiB + 1 wrapped, LF recorded in `ups-size_4k_plus_1.prompt.txt`); spike C-1 (2.1.326 Windows: decomposed entries composed; no TAB or wrapper case) |
| Codex, `bracketed_paste` (macOS) | `Send`: three pastes, `" "`, `lf(t)`, `" "` (§2.6); `AttachPaste`: one paste of `lf(t)` | `trim(lf(t))` | exactly `expected_prompt` | Codex `rust-v0.171.0`: paste maps CR and CRLF to LF and removes controls other than LF and TAB (`paste_input.rs:224-225`), no NFC, no tab expansion, `trim` on submit (`chat_composer.rs:3215-3218`) |
| Codex, `win_keys` (Windows) | `Send`: `" " + lf(compose_marks(t)) + " "` as keys; `AttachPaste`: `lf(compose_marks(t))` as keys (§2.7) | `lf(compose_marks(t))` | exactly `expected_prompt` | spike F-1a oracle, `expected_win_keys` (`paste.rs:697-704`); `check` has refused leading or trailing whitespace (D6), so `trim` is the identity |
| Codex, `positional` (both OSes) | `t` as one argv element after `--` | `lf(t)` | exactly `expected_prompt` | Codex `rust-v0.171.0` `cli/src/main.rs:2546-2548` (spawn) and `:2787-2789` (resume) map CRLF and CR to LF; whether the TUI trims the initial prompt is **unverified** (PR 14) |

**Claude's matching is lenient until PR 13's captures.** The spike measured each Claude transform on separate entries and never on the combinations that tell the rules apart, so `prompt_matches` accepts any subset of `{tab4, nfc}` with or without the wrapper, as the spike's `match_prompt` did (`paste.rs:205-249`). `expected_prompt` returns the form with every transform. PR 13 captures, before its ack tests: a standalone CJK compatibility ideograph (U+FA19: whole-prompt NFC records U+795E); leading and trailing spaces and LF; TAB; and a wrapped body holding TAB and combining marks. It then narrows `prompt_matches` to the observed form. Until then, a prompt Claude records in a form outside this set leaves the input `unconfirmed` (never resent, mvp §2.3); a lenient match can attribute an ack only to an input whose text equals the prompt up to these transforms, and two inputs that both match ack none (mvp §2.3 "Ack").

**Positional argv form** (PR 14 obligation). `codex [OPTIONS] [PROMPT]` (`main.rs:117-130`) parses the prompt as a clap positional (`tui/src/cli.rs:16-18`). Placed last without a terminator, a prompt starting with `-` is parsed as an option, and a prompt equal to a subcommand name (`resume`, `login`) selects that subcommand (`MultitoolCli` dispatches subcommands before the positional; the test `import_remains_an_interactive_prompt`, `:3117-3122`, shows only a word that is not a subcommand). PR 14 therefore builds `codex <flags…> -- <prompt>` and `codex resume <flags…> <session_id> -- <prompt>`, with every option before `--`. That `--` ends option and subcommand parsing in Codex's parser is clap's documented behaviour, **unverified** on Codex: PR 14 checks it with `--version`, `-x`, `resume`, and `login` as prompts against the real binary, and checks separately that a prompt with CR, CRLF, and LF arrives as `lf(t)` in `UserPromptSubmit`.

**Codex paste behaviours that change what is recorded**, with the defined result:

| Case (Codex macOS, `Send`) | Codex `rust-v0.171.0` | Result here |
|---|---|---|
| `trim(lf(t))` starts with `/` and an unknown command name (no `/` in the name), text over 1,000 characters | A paste over `LARGE_PASTE_CHAR_THRESHOLD` (1,000 characters, `chat_composer.rs:489`; `paste_input.rs:256-263`) becomes a placeholder; at Enter, `input_starts_with_space` is read from the unexpanded text, then the text is expanded, trimmed, and refused as an unknown command (`chat_composer.rs:3200`, `:3205-3235`; `slash_input.rs:74-91`). | Delivered as text: the leading space is its own one-character paste, inserted directly (`paste_input.rs:256-270`), so the composer's visible text starts with a space whatever the size of the second paste, and `validate_submission`, `inline_command`, and `bare_command` all leave it as text. P-8 cases at 1,000 and 1,001 characters with `/zzexample` (§5.2 PR5-CL-6). |
| `trim(t)` is a single path token naming a readable image in Codex's working directory (`./diagram.png`) | A paste of more than one character is passed to `handle_paste_image_path` (`paste_input.rs:264-268`); `normalize_pasted_path` trims it and takes a single shell token, a quoted path, or a `file://` URL (`clipboard_paste.rs:231-275`); an existing file with image dimensions is attached instead of inserted (`chat_composer.rs:1327-1351`). | The image is attached and the text is not inserted; the recorded prompt differs, so the input stays `unconfirmed`. Which strings attach depends on files the daemon does not see. Owner item O4 (§7). P-8 pair: the same text with the image present and absent. |
| `trim(t)` starts with `!` | Shell mode when a paste starting with `!` lands at position 0 (`paste_input.rs:37-45`). | Delivered as text: the first paste is the space, so the text paste starts at position 1. |

### 2.5 Rejections

`check` applies these in this order and returns the first class that applies (precedence by class, not by position).

| # | Condition on `t` | `RejectReason` | Claude paste | Codex paste | `win_keys` | `positional` | `at` | Source |
|---|---|---|---|---|---|---|---|---|
| 1 | ESC, or a C0 control other than TAB, CR, LF | `ControlCharacter` | both | both | both | send | first such scalar | prior-art row 4b; mvp §2.3; spike `validate` |
| 2 | DEL (U+007F) or a C1 control (U+0080–U+009F) | `LossyText` | — | both | — | — | first such scalar | Codex's paste path removes them (`sanitize_user_text`, `char::is_control`); owner item O1 |
| 3 | `Send`: `trim(t)` is empty. `AttachPaste`: `t` is empty | `EmptyPrompt` | both | both | both | send | `None` | mvp §2.3, §8 D6; owner item O2 for whitespace-only `Send` on every row |
| 4 | `t` starts or ends with a `White_Space` scalar | `LeadingTrailingWhitespace` | — | — | both | — | 0 when leading, else the start of the trailing run (`t.trim_end().len()`) | mvp §8 D6; spike F-1d |
| 5 | `t` contains TAB | `Tab` | — | — | both | — | first TAB | mvp §8 D6; spike F-1d |

"both" means `Send` and `AttachPaste`; "send" means the row exists only for `Send`.

- Rule 1 makes `ESC[201~` inside the text impossible, so no input can end a bracketed paste early or inject a key record.
- Rule 2 is limited to the path where loss is shown. Claude's handling of DEL and C1 is unknown, and on `win_keys` Codex's key path has no sanitizer (`sanitize_user_text` runs only in `apply_paste`); there they are sent as they are (§2.7) and their arrival is a PR 13 / PR 15 corpus entry. The Windows crossterm fork's special handling covers U+0000–U+001F only (`parse.rs:240-270`), so a C1 record reaches the composer as a character.
- Rule 3 for `AttachPaste` refuses only a paste with no text; a human pasting indentation or blank lines into Claude or Codex macOS keeps them. On `win_keys`, a whitespace-only attach paste is refused by rule 4 (mvp §8 D6 applies to recognised attach pastes).
- Precedence examples: `"\u{0085}"` (C1 and `White_Space`) is `LossyText` on Codex paste and `EmptyPrompt` for `Send` elsewhere; `"\t"` is `EmptyPrompt` for `Send` everywhere; `"\tx"`, `" a\tb"`, and `"\t\u{007f}"` are `LeadingTrailingWhitespace` on `win_keys`; `"\t\u{007f}"` is `LossyText` on Codex paste.
- `SpawnFailed` is never produced here (it is the positional spawn's, mvp §2.3). No allow-list: the spike's `--allow-lossy trim|tab` is not lifted (mvp §8 D6: an explicit transform is additive later).

### 2.6 Bracketed Paste

Each paste is `ESC[200~ + body + ESC[201~`. `nl_cr` maps CRLF, LF, and a lone CR each to one CR (prior-art row 4c, §3 F7); every other scalar is its UTF-8 bytes.

| Row, purpose | `text` |
|---|---|
| Claude, both purposes | `ESC[200~ nl_cr(t) ESC[201~` |
| Codex, `Send` | `ESC[200~ " " ESC[201~` `ESC[200~ nl_cr(t) ESC[201~` `ESC[200~ " " ESC[201~` (three pastes, one part) |
| Codex, `AttachPaste` | `ESC[200~ nl_cr(t) ESC[201~` |

The Codex `Send` framing delivers `/`, `!`, `?`-led text and a trailing `@` token as text whatever the length (§2.4 table, §3 F8). Each space is a one-character paste, which Codex inserts directly (no placeholder, no image check, `paste_input.rs:256-270`), and the middle paste's character count is that of `lf(t)` alone, so Codex's 1,000-character placeholder boundary falls exactly at `lf(t)`'s length. The submit is `SUBMIT` after `submit_delay`, for `Send` only.

The only ESC bytes in `text` are the markers (rule 1), so each paste's two markers are its only escape sequences, all complete inside `text`. `text.len() ≤ t.len() + 38`.

### 2.7 `win_keys`

After `check`: `s = pad + lf(compose_marks(t)) + pad`, where `pad` is one space for `Send` and empty for `AttachPaste`. Then each scalar `c` of `s`, in order, appends:

| `c` | Bytes | Evidence |
|---|---|---|
| U+0020–U+007F | its one byte | mvp §2.7 ("ASCII as UTF-8"); DEL's arrival is a PR 15 corpus entry |
| LF | Shift+Enter key-down and key-up: `ESC[13;28;13;1;16;1_ESC[13;28;13;0;16;1_` (Vk 13, Sc 28, Uc 13, Cs `SHIFT_PRESSED` 16, Rc 1) | spike `NewlineKey::W32` (`paste.rs:427`); arch §5 (`ESC[13;2u` written by the host loses the newline, F-1 `d97ee701`) |
| any other BMP scalar (U+0080–U+FFFF, C1 included, never a surrogate) | `ESC[0;0;<u>;1;0;1_ESC[0;0;<u>;0;0;1_`, `<u>` = the scalar's value in decimal, no leading zeros (Vk, Sc, Cs 0, Kd 1 then 0, Rc 1) | spike `unit_records` (`paste.rs:595-597`); mvp §4 PR 5; arch §5 (U+2764 sent as UTF-8 was lost; records delivered key-downs at the console boundary, phase 3c `adbefa2d`) |
| astral scalar (U+10000–U+10FFFF) | its four UTF-8 bytes | arch §5 (astral samples arrived intact as UTF-8; per-unit records of a surrogate pair would reach Codex's crossterm fork as high-down, high-up, low-down, low-up, `parse.rs:46`, `:258`) |

`check` has refused TAB, C0, and ESC, and `lf` has turned CR into LF. All of it is one `text` part (§3 F6). The submit is `SUBMIT` after `submit_delay`, for `Send` only; the gate's "ready composer only" (mvp §2.7 "Submit") is PR 10's.

**Why the padding is `Send`-only.** The spike pads because "Codex trims both on submit; this disarms `/`, `!`, `?` at the start and an `@` popup at the end" (spike §4.4; arch §5): it protects the submission, not the delivery of any record. An attach paste lands where the human is editing, and a pad there would put spaces into the middle of the prompt (`edit ` + paste `foo.rs` would record `edit  foo.rs`). The cost: an attach paste starting with `/`, `!`, or `?` into an empty composer triggers Codex's composer as the same keys typed by hand would (owner item O5, a refinement of mvp §2.7, which pads both).

### 2.8 Size Bound

`enc(c)` is the bytes §2.6 or §2.7 emits for one scalar: ASCII and DEL 1; LF 1 (paste) or 36 (keys); a BMP scalar 2–3 (paste) or, as a record pair, 32 (`<u>` of 3 digits, U+0080–U+03E7), 34 (4 digits, to U+270F), 36 (5 digits); an astral scalar 4. So `enc(c) ≤ 36 × len(c)` for every scalar `c` alone, with `len` its UTF-8 length.

`compose_marks` changes a cluster by NFC: canonical decomposition of each scalar, canonical reordering, then pairwise composition. Reordering keeps every scalar. A composition replaces a pair `(a, b)` by one scalar; `b` is never ASCII in Unicode's composition data, so `enc(a) + enc(b) ≥ 1 + 32`, and the composite costs at most 36: one composition adds at most 3 bytes, and only when `a` is ASCII. Hence `enc(compose_marks(x)) ≤ Σ_c (enc(D(c)) + 3 × k(c))`, where `D(c)` is the canonical decomposition of `c` (or `c` itself outside a composed cluster or for a kept CJK compatibility ideograph) and `k(c)` counts the scalars of `D(c)` that compose with some ASCII scalar. The lemma `enc(D(c)) + 3 × k(c) ≤ 36 × len(c)` for every scalar is checked exhaustively (PR5-CL-12, using `char::decompose_canonical` and `char::compose`); with it, `win_keys` text is at most `36 × t.len() + 2`. Bracketed paste is at most `t.len() + 38` (three pastes, 36 marker bytes, 2 pads). With the 1-byte submit, `MAX_EXPANSION` 36 and `MAX_OVERHEAD` 39 cover every row and purpose, and the writer's default (pty §5, 36 × + 64) is above them.

## 3. Structure Options

| Fork | Option | Precedent | Verdict |
|---|---|---|---|
| **F1. Module layout** | A. Grow `input.rs` with the encoders and the table | — | Rejected: the table is read by gate, supervisor, adapters, and `/api/adapters`, not only by input code. |
| | B. `core::input::encode` (non-`mod.rs` submodule) + `core::adapter` | spike `paste.rs` (encoders) next to its `input_encoding` table | **Adopted.** mvp §3 places both in `core`; no existing path moves. |
| | C. A separate crate | — | Rejected: `core` already owns `InputEncoding` and `RejectReason`; a crate would split the contract from its rules. |
| **F2. Unicode dependencies** | A. `unicode-normalization =0.1.26` + `unicode-segmentation =1.13.3` | spike pins (`Cargo.toml` at `03b00b8`); mvp §3 | **Adopted.** The spike's composition tests ran on these exact versions; one small transitive crate. |
| | B. ICU4X (`icu_normalizer`, `icu_segmenter`) | — | Rejected: data providers and a much larger tree for the same two operations. |
| | C. Hand-written tables (the spike's `marks.rs`, Mn/Me/Cf, Unicode 16.0) | spike `src/marks.rs` | Rejected: records cover every non-ASCII BMP scalar (F10), so no category table is needed; NFC by hand is not justified (prior-art rule). |
| **F3. Encoded unit shape** | A. A list of writes (`Vec<Vec<u8>>`) | spike `encode`, `encode_win_keys_with` | Rejected: the writer takes at most 8 parts (pty §2.3), and a multi-line prompt has more lines. |
| | B. `Encoded { text, submit: Option<Submit { delay, bytes }> }` | pty §2.3 "Send path" (`[text, Pause, submit]`) | **Adopted.** Maps one-to-one onto `WriteUnit` parts. |
| | C. `core` returns a `WriteUnit` | — | Impossible: `pty-runtime` depends on `core` (mvp §3 dependency direction). |
| **F4. Who waits the submit delay** | A. The supervisor sleeps, then submits the CR as a second unit | spike `paste`, `type_keys` (`thread::sleep`, `paste.rs:388`, `:793`) | Rejected: a human write or a VT reply could land between the text and its submit (mvp §2.3 asks for one unit). |
| | B. The delay is data (`Submit::delay`) and the writer runs it as `Part::Pause` inside the unit | herdr keeps its delay inside one submission (`src/pty/actor.rs:155-163`, pty §3 "Reply placement") | **Adopted.** Already the writer's contract (pty §2.3); `core` stays clock-free. |
| **F5. Invalid input** | A. Strip or replace controls | Zed, Alacritty, xterm.js, libghostty, tmux (prior-art row 4b) | Rejected: changes the prompt silently (invariant 4). |
| | B. Reject with zero bytes | prior-art row 4b; spike `validate` | **Adopted.** |
| **F6. `win_keys` write granularity** | A. One write per line and one per newline key | spike `encode_win_keys_with` (each queued in one `WriteMsg::Data`) | Rejected: needs more parts than the writer allows; its only purpose, keeping each sequence whole, holds with one part too. |
| | B. One part for all text | — | **Adopted.** Every record lies inside the one write. Not yet run against real Codex: P-7 runs it, including a near-limit record sequence (PR5-CL-17). A fallback that splits the text needs the encoder to return safe split offsets between sequences (an `Encoded` field) and the writer to accept that many parts or a chunked part; neither exists now. |
| **F7. Bracketed paste newline** | A. `cr` | xterm.js, tmux (prior-art row 4c); spike default | **Adopted.** The spike's sweep settled the measured variable: Claude arrived exact in `cr`, `lf`, and `crlf` on macOS (A-6) and Windows (C-1, 30 cases), recording LF; Codex's paste path maps CR and CRLF to LF (source). `cr` is also what Enter sends. |
| | B. `lf` / C. `crlf` | Alacritty, Zed, libghostty (LF); herdr on Windows (CRLF) | Rejected: no case where they do better; one form keeps one golden set. |
| **F8. Codex `/`, `!`, `?`, `@` and the large-paste placeholder (macOS `Send`)** | A. One paste of the text as is | mvp §2.7 row as written | Rejected: a `/`-led prompt is dispatched or refused as a command, a `!`-led one enters shell mode, and an `@` token at the end opens the file popup (§2.4). |
| | B. One paste padded with a space on each side | spike F-1c (`win_keys` padding) | Rejected: over 1,000 characters the padded paste becomes a placeholder whose leading space Codex does not see at validation, so `/zzexample …` is refused (`chat_composer.rs:3200`, `:3205-3235`). |
| | C. Three pastes: `" "`, the text, `" "` | — | **Adopted.** The visible leading space survives any length; trim removes both spaces, so the expected form is unchanged (owner item O3). |
| | D. Reject such prompts | — | Rejected: legitimate prompts ("/review の件") would be refused. |
| **F9. DEL and C1 controls** | A. Accept everywhere (spike `validate` allowed `0x7f` and C1) | spike `validate` | Rejected for Codex paste: its paste path removes them (`sanitize_user_text`). |
| | B. Reject as `LossyText` on Codex paste only | — | **Adopted** (owner item O1). Loss is shown there only; Claude, `win_keys`, and positional keep the accepted set, with their arrival as corpus entries. |
| | C. Reject everywhere | — | Rejected: no evidence of loss on the other paths. |
| | D. A new `RejectReason` variant | — | Rejected: a public enum change for a case with no known use. |
| **F10. `win_keys` record scope** | A. Records only for BMP Mn, Me, Cf | spike phase 3c `--unicode-records marks` | Rejected: U+2764 (So) sent as UTF-8 was lost (arch §5). |
| | B. Every non-ASCII BMP scalar | mvp §2.7 | **Adopted.** |
| | C. Every non-ASCII scalar, astral as surrogate-unit records | — | Rejected: the crossterm fork pairs surrogates wrongly from separate records (`parse.rs:46`, `:258`). |
| **F11. `win_keys` newline key** | A. `ESC[13;2u` (`csi-u`) | Codex `windows_key_sequence.rs` reassembly | Rejected: written by the host, every such newline was lost (spike F-1, `d97ee701`; arch §5). |
| | B. Shift+Enter key records (`w32`) | spike `NewlineKey::W32` | **Adopted.** |
| **F12. `win_keys` composition** | A. Whole-prompt NFC | — | Rejected: turns U+FA19 into U+795E. |
| | B. NFC per cluster holding a mark, CJK compatibility ideographs kept | spike `compose_marks` | **Adopted.** |
| | C. None | — | Rejected: Codex lost combining marks (`が` → `か`, spike C-1). |
| **F13. Capability table form** | A. A function over strings or `cfg` | spike `input_encoding(agent: &str, os: &str, …)` | Rejected: typos compile; `cfg` hides rows from tests on the other OS (F-1f). |
| | B. A `const` array keyed by `(Agent, HostOs)`, an exhaustive `match`, and `#[non_exhaustive]` rows | mvp §2.7 ("a const table in `crates/core`") | **Adopted.** No caller can build an unsupported row. |
| | C. A runtime configuration file | — | Rejected: capability changes come with fixtures and tests; no config surface in the MVP. |
| **F14. Public exposure of the table** | A. `ts-rs` / `schemars` derives on `Capabilities` now | — | Rejected: PR 10, 13, 14 add columns (hook injection, readiness modes, resume); freezing a wire type now fixes its shape before they are designed. |
| | B. Internal Rust type now; PR 10 defines the `GET /api/adapters` body from it | http-surface §2.2 route table (`ReadAdapters`, PR 10) | **Adopted.** PR 5's columns map to wire fields directly (`agent`, `os`, `pty_encoding`, `first_prompt`, `submit_delay_ms`). |
| **F15. Where the expected form lives** | A. In PR 10's ack code | — | Rejected: it would duplicate the encoder's transforms, and the encoder could not be tested against it. |
| | B. In `core::input::encode`, next to the encoder | spike `expected_win_keys`, `match_prompt` in `paste.rs` | **Adopted.** PR 10 calls `prompt_matches`; attribution stays PR 10's. |
| **F16. The purpose in the encoder** | A. One encoding for `Send` and `AttachPaste`, differing only in the submit | mvp §2.7 ("the same text encoding applies to `send` and to recognised attach pastes") | Rejected: the padding and the whitespace-only rejection would corrupt or drop a paste into a composer being edited (§2.5, §2.7). |
| | B. `Purpose` reaches `check` and `encode`; the submit, the padding, and the whitespace-only rejection are `Send`'s | — | **Adopted.** The character encoding (records, newline key, composition, `cr`) and the content rejections stay identical for both purposes. |

## 4. Same-Class Inventory

Later PRs that call these functions or read the table, and the seam each uses.

| PR | Uses | Seam |
|---|---|---|
| 10 supervisor | `capabilities(agent, host_os)` at reservation; `check(agent, encoding, Send, text)` for the input's rejection (`InputInput::Rejected(reason)` from `queued`, mvp §2.3); `encode(caps, text, Send)` at write time, mapped to `WriteUnit` as in §2.3, with `Guard::bracketed_paste` when `pty_encoding` is `BracketedPaste`; `GateView::needs_bracketed_paste` from the row; `InputCreated::encoding` = `first_prompt` for a positional first prompt, else `pty_encoding`; `prompt_matches` inside ack attribution; `GET /api/adapters` built from `CAPABILITIES`; `check(Codex, Positional, Send, text)` before `launch_reserved` for a Codex `--prompt`. Tests: a FakePty unit of `Encoded` whose part boundaries and per-part write calls match §2.3's mapping (the writer-side half of the escape-safety claim). | `core::adapter`, `core::input::encode` |
| 11 attach | A recognised paste: decode UTF-8, `encode(caps, text, AttachPaste)`, one `WriteUnit { Attach, [Bytes(text)] }`; a `Rejection` drops the paste with zero bytes (mvp §4 PR 11). Outer-terminal framing and recognition are PR 11's. Tests: the attach unit has one part and no submit (CL-19's attach half). | `encode` with `Purpose::AttachPaste` |
| 13 Claude adapter | Bracketed paste send through PR 10; the captures of §2.4 (compatibility ideograph, spaces and LF at the ends, TAB, wrapped body with TAB and marks), DEL and C1, and a pasted prompt starting with `/`, `!`, `#`; then narrows `prompt_matches` and, if needed, `check` for the Claude rows. | Claude rows; §2.4, §2.5 |
| 14 Codex adapter | `win_keys` for send and attach paste on Windows; the positional argv form with `--` and its checks (§2.4); the positional expected form, trimming included (fixture); `-c tui.disable_paste_burst=true` (its own flag column); herdr's paste-boundary key as the fallback if burst-off falls short (mvp §4 PR 13 / 14), which would change §2.7 only. FakeCli attach paste tests on `windows-latest` (mvp "Why the first tests run in PR 15"). | Codex rows; §2.4, §2.7 |
| 15 real Windows host | P-7 (corpus F through `send`, plus a near-limit record sequence with one submit, PR5-CL-17) and P-9 (attach paste) with real Codex; DEL and C1 through `win_keys`. A failure changes only the encoder, the table, or the attach paste path (mvp §4). | `encode` output as written by PR 10 / 11 |
| 19 PWA input | Shows `rejected` with its `RejectReason` from `input_state_changed`; may read `GET /api/adapters` for hints. Never encodes. | Events; `/api/adapters` (PR 10) |
| 22 fuzz | Targets `encode` (every row × both purposes: no panic, `Err` ⇔ `check` `Err`, the per-encoding escape grammar, the size bound, the reference-model decode equal to `expected_prompt` for `Send`), `check`, `expected_prompt`, and `prompt_matches` over every `(Agent, InputEncoding, Purpose)` including unsupported ones (total, no panic; `prompt_matches(…, expected)` true whenever `expected_prompt` is `Some`). Seeds: the corpus of §6. | `core::input::encode` |
| 23 real hosts | P-6 and P-8 with the corpus of §6; P-8 adds corpus F's trigger entries, the 1,000 / 1,001-character `/zzexample` pair, and the image-path pair (present and absent). | — |

## 5. Purpose Layer and Plan Layer

### 5.1 Purpose Layer: Acceptance Criteria

| ID | Criterion (EARS) | Observable result / failure | How to check |
|---|---|---|---|
| PR5-P-1 | When a prompt from the corpus is encoded for any table row and purpose, the bytes shall equal the fixed sequences for that encoding (markers, CR, unit records, Shift+Enter records, padding, paste framing). | Pass: byte-equal goldens. Fail: any differing byte. | `cargo test -p chaldea-core` on `macos-latest` and `windows-latest`. |
| PR5-P-2 | When an accepted prompt's `Send` bytes are decoded by a model of the CLI's documented input handling, the result shall equal the prompt's expected form. | Pass: equal for every corpus entry and generated input. Fail: a lost, added, or changed scalar. | Same; proptest. |
| PR5-P-3 | If a prompt holds a character the encoding or CLI is shown to drop or reinterpret, or a sent prompt is empty, then the encoder shall return the reason and no bytes. | Pass: the reason of §2.5. Fail: `Ok`, another reason. | Same. |
| PR5-P-4 | When the real CLIs receive these bytes, each prompt shall arrive once and equal its expected form (mvp P-6, P-7, P-8, P-9). | Real hosts only. | PR 15, PR 23; not checked in PR 5. |

### 5.2 Plan Layer: Claims

Tests named `encode::…` are unit tests in `crates/core/src/input/encode.rs`; `adapter::…` in `crates/core/src/adapter.rs`; `input_encode::…` in `crates/core/tests/input_encode.rs`. Every test runs on both CI runners (no `cfg`). Goldens are of two kinds, kept in separate tables: **measured** sequences the spike wrote and observed (paste markers and CR; the record format for U+200D, U+FE0F, U+2764, U+3099, U+309A at the console boundary, U+200D and U+309A in Codex; the `w32` Shift+Enter pair; the spike's Claude testdata), and **derived** sequences this design computes from those formats (every other BMP record such as `日`; the `Send` / `AttachPaste` padding; Codex's three-paste framing; one-part layout).

| ID | Claim (EARS) | Source | Test that falsifies it | Mutation it kills |
|---|---|---|---|---|
| PR5-CL-1 | If `check` rejects a text for a purpose, then `encode` shall return that `Rejection` and no `Encoded`; and when `check` accepts it, `encode` shall return `Ok`. | mvp §4 PR 5 ("zero bytes on rejected input"); mvp §2.3 | proptest `input_encode::encode_err_iff_check_err` (arbitrary `String`s with forbidden scalars inserted at a random position, every row and purpose) | `encode` validating only the first line; `encode` skipping `check` for `AttachPaste` |
| PR5-CL-2 | If a text contains ESC or a C0 control other than TAB, CR, LF anywhere, then `check` shall return `ControlCharacter` for every supported combination, `at` naming its first occurrence. | prior-art row 4b; mvp §2.3 | `encode::controls_rejected` (each of U+0000–U+001F except TAB, CR, LF, at start, middle, end; literal `ESC[201~` and `ESC[0;0;65;1;0;1_` in the text); proptest `input_encode::control_anywhere_rejected` | ESC omitted from the set; NUL accepted; scan stopping at the first LF |
| PR5-CL-3 | If a text for Codex's bracketed paste contains DEL or a C1 control and no rule-1 character, then `check` shall return `LossyText`; the same text shall pass rule 2 on Claude, `win_keys`, and positional. | §2.5 rule 2; Codex `sanitize_user_text` | `encode::del_and_c1_on_codex_paste` (all 33 scalars U+007F–U+009F, each alone between `a` and `b`, both purposes; ESC present → `ControlCharacter`); `encode::del_and_c1_elsewhere_accepted` (the same 33 texts `Ok` on the other rows, with goldens: DEL raw on `win_keys`, U+0085 as a record) | DEL accepted on Codex paste; the rule applied to Claude or `win_keys`; precedence reversed |
| PR5-CL-4 | If `trim(text)` is empty for `Send`, or `text` is empty for `AttachPaste`, then `check` shall return `EmptyPrompt` before rules 4 and 5; and a non-empty whitespace-only attach paste shall pass rule 3. | mvp §2.3; §8 D6; §2.5 | `encode::empty_and_blank` (`""` both purposes; `" "`, `"\u{3000}"`, `"\n"`, `"\r\n"`, `"\t"`, `" \t "` → `EmptyPrompt` for `Send` on every row; `"    "` and `"\n\n"` `Ok` for `AttachPaste` on Claude and Codex paste and `LeadingTrailingWhitespace` on `win_keys`; `"\u{0085}"` → `LossyText` on Codex paste, `EmptyPrompt` for `Send` elsewhere) | Whitespace-only rejected for `AttachPaste`; `win_keys` reporting `LeadingTrailingWhitespace` for a sent `" "`; rule 2 after rule 3 |
| PR5-CL-5 | If a `win_keys` text starts or ends with a `White_Space` scalar, then `check` shall return `LeadingTrailingWhitespace` with `at` 0 for a leading run and the trailing run's start otherwise; if it contains TAB and no such whitespace, `Tab` at the first TAB; and the other rows shall accept both. | mvp §8 D6; spike F-1d | `encode::d6_rejections` (`"\u{3000}一行目\n二行目"` at 0, `"一行目\n二行目\n"` at 19, `"a  "` at 1, `"\u{a0}a"` at 0, `"\tx"` and `" a\tb"` → `LeadingTrailingWhitespace` at 0, `"\t\u{007f}"` → `LeadingTrailingWhitespace`, `"列1\t列2"` → `Tab` at 4; the same texts pass rules 4 and 5 on the other rows) | ASCII-only whitespace test (U+3000 accepted); trailing LF accepted; `at` at the last scalar; `Tab` before `LeadingTrailingWhitespace`; D6 applied to macOS |
| PR5-CL-6 | When a text is encoded for a `BracketedPaste` row, `text` shall be the framing of §2.6 for its row and purpose, with each CRLF, LF, and lone CR as one CR and no other ESC byte. | mvp §2.7 (newline `cr`); prior-art rows 4a, 4c; §3 F7, F8, F16 | measured goldens `input_encode::bracketed_measured` (spike corpus with `cr`, Claude); derived goldens `input_encode::bracketed_derived` (`"a\nb\r\nc\rd"` → `ESC[200~a\rb\rc\rdESC[201~` for Claude both purposes and Codex `AttachPaste`; `"/review の件"` → `ESC[200~ ESC[201~ESC[200~/review の件ESC[201~ESC[200~ ESC[201~` for Codex `Send`; `/zzexample ` + 989 and + 990 `x`, whose `lf(t)` is 1,000 and 1,001 characters, with the middle paste's character count asserted); proptest `input_encode::bracketed_shape` | LF kept; CRLF → two CRs; pads inside the text paste; pads on attach; one paste for Codex `Send` |
| PR5-CL-7 | When a text is encoded for a `WinKeys` row, every scalar U+0020–U+007F shall be its byte, every other BMP scalar a key-down and key-up record pair carrying its value in decimal without leading zeros, every astral scalar its UTF-8 bytes, and each newline one Shift+Enter record pair, with one space before and after for `Send` only. | mvp §2.7; mvp §4 PR 5; arch §5 (U+2764) | measured goldens `input_encode::win_keys_measured`: U+200D → `ESC[0;0;8205;1;0;1_ESC[0;0;8205;0;0;1_`, U+FE0F 65039, U+2764 10084, U+3099 12441, U+309A 12442, the `w32` pair; derived goldens `input_encode::win_keys_derived` over corpus F (spike `paste.rs:833-879`), compared as whole byte strings: `日` 26085, U+FA19 64025, U+00E9 233 (3 digits), `small_kana_mark` U+31F7 U+309A → `" "` + 12791 pair + 12442 pair + `" "`, `𠮷野家`, multi-line LF / CRLF / blank line, `" /review の件 "`, and each entry for `AttachPaste` without pads; proptest `input_encode::win_keys_byte_classes` (no byte < 0x20 except ESC, no 2- or 3-byte UTF-8 sequence, every ESC starts one of the two record shapes) | Records only for Mn/Me/Cf (U+2764 as UTF-8); astral as surrogate records; `csi-u` newline; zero-padded or hex `<u>`; key-up omitted; pads dropped for `Send` or kept for `AttachPaste` |
| PR5-CL-8 | When a `win_keys` text holds a cluster with a combining or extending scalar, that cluster shall be NFC-composed with CJK compatibility ideographs kept, and every other cluster shall be unchanged. | mvp §2.7; spike F-1a, `compose_marks_per_cluster` | goldens `encode::compose_marks_cases` and fixed `expected_prompt` values: `か\u{3099}` → `が`, `e\u{301}` → `é`, U+09C7 U+09BE → U+09CB, U+FA19 unchanged, U+FA19 U+0301 unchanged, U+31F7 U+309A unchanged, `U+FA19 か\u{3099}` → `U+FA19 が`, U+1100 U+1161 (Hangul L V, no mark) unchanged | Whole-prompt NFC (U+FA19 → U+795E, U+1100 U+1161 → U+AC00); a class-only rule (U+09BE has class 0); no composition; a mark dropped (fixed expected values differ) |
| PR5-CL-9 | `encode` shall emit every escape sequence whole inside `text` and none in the submit: for bracketed paste only the markers, for `win_keys` only complete record pairs. | mvp §4 PR 5 (spike F-3d), encoder half; arch §5 conhost | proptest `input_encode::escapes_complete` (every ESC in `text` begins a sequence of the encoding's grammar that ends inside `text`; `text` never ends inside one; `submit.bytes == b"\r"` exactly) | Submit CR appended to `text`; a truncated record at the end; a marker missing its final byte. The writer half (one write call per part) is PR 10's FakePty test (§4). |
| PR5-CL-10 | When `purpose` is `Send`, `encode` shall return `submit = Some { delay: row.submit_delay, bytes: b"\r" }`; when it is `AttachPaste`, `None`. | mvp CL-19 (second half); §2.3; §2.7 "Submit" | `encode::submit_follows_the_row` (an in-crate test row with `submit_delay` 123 ms → `Submit { delay: 123 ms, bytes: b"\r" }` exactly); proptest `input_encode::submit_only_for_send` (every table row, corpus and generated) | Submit added for attach; delay from a constant instead of the row; a CRLF, a second CR, or a `w32` Enter record as submit |
| PR5-CL-11 | For every text accepted for `Send` and every row, decoding `text` with the CLI model shall give `expected_prompt`, and `prompt_matches(…, expected_prompt(…))` shall be true. | invariant 4; arch §5; mvp §2.3 "Ack" | proptest `input_encode::model_decode_equals_expected` (generator weighted over ASCII, kana, kanji, U+3000, combining marks, ZWJ, VS15/16, keycaps, flags, astral, CJK compatibility ideographs, CR/LF/CRLF, TAB, DEL, C1, U+0344, U+FB2C); the corpus run `input_encode::corpus_model` | A dropped, duplicated, or reordered scalar in the encoder only; a record with the wrong unit; a pad not trimmed in Codex's model. Mutations shared by encoder and expected form (`compose_marks`, `lf`) are killed by PR5-CL-8 and PR5-CL-15's fixed values. |
| PR5-CL-12 | For every text, row, and purpose, `encode`'s output shall be at most `MAX_EXPANSION × text.len() + MAX_OVERHEAD` bytes, text and submit together. | pty §5, D8 ("PR 5 bounds the encoded size"); §2.8 | `input_encode::size_lemma_every_scalar` (§2.8's lemma for all 1,112,064 scalars); `input_encode::size_bound_every_scalar` (every scalar alone, between `a` and `b`, and after `a` with U+0301 following, every row and purpose); proptest `input_encode::size_bound`; `input_encode::max_prompt_worst_case` (`"a" + "\n" × (MAX_PROMPT_BYTES − 2) + "a"` for `win_keys` `Send`, asserted equal to `36 × MAX_PROMPT_BYTES − 67` bytes and within the bound) | A bound or lemma that does not hold for some scalar (the exhaustive tests report it); a newline record emitted twice. Zero padding and a second submit are PR5-CL-7 and PR5-CL-10's. |
| PR5-CL-13 | The encoding shall be chosen by `(agent, os)` alone: Claude on both OSes and Codex on macOS shall have `pty_encoding = BracketedPaste`, Codex on Windows `WinKeys`; Codex shall have `first_prompt = Positional` on both OSes and Claude `BracketedPaste`; every row's `submit_delay` shall be 300 ms. | mvp CL-19 (first half); spike F-1f; mvp §2.7; §8 D5; prior-art row 4e | `adapter::table_rows` (all four rows asserted on every host, so a `cfg` choice fails on the other runner); `adapter::table_and_match_agree`; `adapter::row_invariants` (`pty_encoding ≠ Positional`; `first_prompt` is `Positional` or `pty_encoding`). No function of `core::adapter` or `encode` takes a mode or VT parameter (signature review). | `cfg!(windows)` in `capabilities`; Codex macOS → `WinKeys`; Claude → `Positional`; a row's delay 0 or 100 ms |
| PR5-CL-14 | For Claude, `prompt_matches` shall accept the recorded prompt when it equals `T(lf(text))` or `wrap(id, T(lf(text)))` for a subset `T` of `{tab4, nfc}`, and reject every other difference; PR 13 narrows the accepted set to its captures. | arch §5; spike A-6, C-1, `match_prompt`; §2.4 | `input_encode::claude_recorded_prompts` (the four spike testdata files: ascii → TAB expanded, both decomposed → NFC, 4 KiB + 1 → wrapper); `encode::claude_mismatches` (2-space TAB, NFD recorded for composed text, wrapper ids differing, `id=""`, `id="a b"`, wrapper around a shorter body, a recorded CR, a trimmed prompt) | Wrapper ids not compared; `id` charset not checked; NFC applied to the recorded side; TAB expansion width 2 |
| PR5-CL-15 | For Codex, `prompt_matches` shall accept only `expected_prompt` exactly: `trim(lf(text))` for bracketed paste, `lf(compose_marks(text))` for `win_keys`, `lf(text)` for positional. | Codex source (§2.4); spike F-1a oracle; mvp §2.3 "Ack" | `encode::codex_expected_forms` (fixed values over corpus F; `"a\r\nb\rc"` positional → `"a\nb\nc"`; `" a "` positional → `" a "`; a Codex prompt with a wrapper rejected; NFC-only form rejected for `win_keys`) | Wrapper accepted for Codex; NFC applied to `win_keys`' expected form (U+1100 U+1161); trim missing on macOS; positional CR kept |
| PR5-CL-16 | `check`, `expected_prompt`, and `prompt_matches` shall be total over every `(Agent, InputEncoding, Purpose)`: an unsupported combination yields `Rejection { LossyText, None }`, `None`, and `false`, and no input panics. | §2.1 rule 5; §2.3 | `encode::unsupported_combinations` ((Claude, WinKeys), (Claude, Positional), (Codex, Positional, AttachPaste)); proptest `input_encode::total_functions` (arbitrary strings, every combination) | A `panic!`/`unreachable!` for an unsupported pair; `expected_prompt` returning Claude's form for `(Claude, WinKeys)` |
| PR5-CL-17 | When P-7 sends a near-limit `win_keys` text, Codex shall record one `UserPromptSubmit` equal to its expected form after one submit. | §3 F6; spike R12 | PR 15 P-7 entry: about 256 KiB of non-ASCII BMP text (about 3 MiB of records) in one part, then one CR | One-part writes split by conhost reads in a way that loses a record (observed on the real host only) |
| PR5-CL-18 | `core` shall build and pass its tests without tokio, filesystem, network, process, or clock access, with the two new dependencies. | mvp CL-24 | CI "Check core dependencies"; `cargo clippy -p chaldea-core` with `crates/core/clippy.toml`; `cargo tree -p chaldea-core -e normal` lists `unicode-normalization v0.1.26`, `unicode-segmentation v1.13.3`, `tinyvec` and nothing on the denylist | A `thread::sleep` for the submit delay in `core`; a dependency that pulls tokio |

Coverage of the obligations handed to PR 5:

| Obligation | Claims |
|---|---|
| mvp §4 PR 5: zero bytes on rejected input | PR5-CL-1–5 |
| mvp §4 PR 5: never splits an escape sequence (spike F-3d) | PR5-CL-9 (encoder), PR 10's FakePty test (writer, §4), PR5-CL-17 (conhost) |
| mvp §4 PR 5: `win_keys` byte sequences and Shift+Enter records over corpus F | PR5-CL-7, -8 |
| mvp §4 PR 5: submit delay | PR5-CL-10, -13 |
| mvp §4 PR 5: encoding by `(agent, os)`, never by DECSET 2004 (CL-19 first half, F-1f) | PR5-CL-13 |
| CL-19 second half: an attach paste never carries a submit | PR5-CL-10 (encoder half; PR 11 tests the attach unit) |
| §2.3: what the encoder reports (`RejectReason`, text and submit as one unit) | PR5-CL-1–5, -9, -10 |
| §2.7 "Text encoding", "First prompt", "Submit" | PR5-CL-6, -7, -10, -13, -15 |
| §2.7 `win_keys` paragraph | PR5-CL-5, -7, -8, -9, -10 |
| §8 D6 | PR5-CL-4, -5 |
| arch §5 prompt delivery, Bracketed Paste vs keyed input, U+2764; AGENTS.md invariant 4 | PR5-CL-6, -7, -11, -14, -15 |
| "Open items of arch §5": unit tests of Codex keyed input | PR5-CL-5, -7, -8, -11 |
| pty D8: PR 5 bounds the encoded size | PR5-CL-12 |
| CL-24 with new dependencies | PR5-CL-18 |

## 6. Tests and Completion Commands

**Corpus** (`crates/core/tests/input_encode.rs`, one `const` table with ids): the spike's A-6 corpus (`paste.rs:70-111`: ASCII with TAB, Japanese, half-width kana, astral, emoji, ZWJ family, flag, `❤️`, decomposed kana and Latin, multi-line LF and CRLF, 4 KiB + 1); corpus F (`paste.rs:833-879`): its required entries and `small_kana_mark` (`ㇷ゚`, U+31F7 U+309A) are accepted on `win_keys` with fixed bytes and expected form, and only `lossy_trim` (`LeadingTrailingWhitespace`) and `lossy_tab` (`Tab`) are `win_keys` rejections; and these additions: U+09C7 U+09BE; U+FA19 U+0301; U+1100 U+1161; U+0344; U+FB2C U+05B0; keycap `1\u{FE0F}\u{20E3}`; U+E0100 after a kanji (astral variation selector); U+1D165; each C0 control; ESC alone and `ESC[201~` inside text; DEL and all of U+0080–U+009F; empty; `" "`, `"\u{3000}"`, `"\n"`, `"\t"`, `"    "`, `"\n\n"`; leading and trailing space, U+3000, LF; TAB in the middle; the precedence cases of §2.5; a lone CR at the end; mixed CR / LF / CRLF; `/zzexample` at 1,000 and 1,001 characters; `./diagram.png`; `MAX_PROMPT_BYTES` of mixed BMP text. Each entry lists, per row and purpose, `Ok(golden bytes)` or `Err(reason, at)`, its expected prompt, and whether its golden is measured or derived (§5.2).

**Reference model** (test-only, in `input_encode.rs`, independent of the encoder's code): `model_paste(agent, purpose, bytes)` requires exactly the framing of §2.6 (one paste, or three for Codex `Send`), refuses any ESC other than the markers and any LF or CRLF inside a paste, maps CR to LF, then applies Claude's `tab4 ∘ nfc` or Codex's `trim`. `model_keys(bytes)` reads ASCII bytes, astral UTF-8, and `ESC[Vk;Sc;Uc;Kd;Cs;Rc_` records; it requires each key-down to be followed directly by its key-up with the same fields, refuses a 2- or 3-byte UTF-8 sequence, maps Vk 13 with `SHIFT_PRESSED` to LF and `Uc` to its scalar, then applies Codex's `trim`. `nfc`, `tab4`, `trim`, and `lf` are restated in the test from §2.4; `compose_marks` is not (its fixed values, PR5-CL-8, pin it).

**Proptests.** `proptest` (already a dev-dependency) with the default case count; strategies are weighted unions of the character classes listed in PR5-CL-11 plus arbitrary `String`s, over every row and both purposes. A counterexample is fixed in the code or reported; the generator is never narrowed to avoid it.

**Mutation check.** Before merging, each mutation in §5.2's last column that a PR 5 test owns is applied by hand, the named test is seen failing, and the change is reverted; the PR description lists the result per claim.

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

**D1. The purpose reaches the encoder** (§3 F16). The character encoding and the content rejections are the same for `Send` and `AttachPaste`; the submit, the padding (Codex), and the whitespace-only rejection are `Send`'s, because only a submission into an empty composer needs them.

**D2. `win_keys` writes all text as one part** (§3 F6), not one write per line as in the spike's verified runs. The writer's part limit leaves no other shape for a multi-line prompt, and escape safety needs only that no sequence crosses a write. P-7's near-limit entry is its first real run (PR5-CL-17); a fallback needs the encoder changes stated in F6.

**D3. The capability table's wire shape is PR 10's** (§3 F14). PR 5's Rust type is internal and its rows cannot be built outside `core`.

**D4. Claude's ack matching accepts any subset of its known transforms until PR 13's captures** (§2.4). This keeps the spike's measured behaviour and turns an unknown Claude rendering into a match rather than an `unconfirmed` input; PR 13 narrows it.

**D5. The positional prompt is passed after `--`** (§2.4), so no prompt can be read as a Codex option or subcommand; PR 14 verifies the parser and the CR/LF conversion on the real binary.

### Owner Items

These change which inputs are accepted, which `RejectReason` is recorded, or the bytes a CLI receives, relative to mvp §2.3, §2.7, §8 D6, and prior-art row 4b. None changes a type; each is a reversible refinement (loosening or switching back is a code change in `core::input::encode` with no stored-data migration, since a reason already recorded keeps its meaning).

**O1. DEL and C1 controls on Codex's bracketed paste are rejected as `lossy_text`** (§2.5 rule 2, both purposes).
- Options: (a) reject (adopted); (b) accept and model Codex's removal in the expected form, so the prompt arrives without them and is still acknowledged; (c) accept unchanged, so the ack never matches and the input is `unconfirmed`.
- Gains / losses: (a) keeps prior-art 4b's "no silent change" at the cost of refusing text a human could paste natively (Codex would strip it); (b) delivers but changes the prompt silently; (c) delivers a changed prompt and leaves an attention item.
- Recommendation: (a). Changes prior-art row 4b's set and mvp §2.3's `rejected` list for this row.

**O2. A whitespace-only `send` is `empty_prompt` on every row** (§2.5 rule 3): Claude and Codex paste and the positional prompt gain this rejection, and Codex on Windows records `empty_prompt` instead of the trim rejection for `" "`.
- Options: (a) `empty_prompt` for every row's `send` (adopted); (b) only Codex on Windows, as mvp §2.3 lists, with `leading_trailing_whitespace` for `" "` there.
- Gains / losses: (a) no CLI is known to submit whitespace-only text, so a send that cannot be acknowledged is refused up front instead of waiting 30 s into `unconfirmed`; it refuses a send Claude might in principle accept; (b) keeps mvp's wording and leaves such sends `unconfirmed`.
- Recommendation: (a). Attach pastes are not affected (rule 3 refuses only an empty paste there).

**O3. Codex's bracketed-paste `send` is three pastes: a space, the text, a space** (§2.6, §3 F8). mvp §2.7 says only "bracketed paste; `cr`".
- Options: (a) three pastes (adopted); (b) one paste of the text (mvp as written): `/`-led prompts run or are refused as commands, `!`-led ones enter shell mode; (c) one padded paste: correct up to 1,000 characters, refused as an unknown command above (§2.4).
- Gains / losses: (a) delivers every prompt as text at any length; 26 more bytes per send; unverified on a real Codex until P-8 runs the trigger and 1,000 / 1,001 cases.
- Recommendation: (a).

**O4. A Codex macOS `send` whose text is a single path to a readable image in Codex's working directory attaches that image** (§2.4).
- Options: (a) accept and define it: the image is attached, the recorded prompt differs, the input stays `unconfirmed` and is never resent (adopted for now); (b) reject every send whose trimmed text is a single shell token containing no whitespace, a quote pair, or a `file://` URL, which also refuses single-word prompts such as `continue`; (c) send each scalar of such text as its own one-character paste, which Codex never checks for an image (`char_count > 1`, `paste_input.rs:264`), at about 13 bytes per character and with Codex's handling of thousands of paste events unmeasured.
- Gains / losses: (a) no prompt refused, a rare side effect (an attachment) made visible as `unconfirmed`; (b) safe but refuses common one-word prompts; (c) delivers as text but is unverified and expensive for long tokens.
- Recommendation: (a) for the MVP, with P-8's present / absent pair recording what Codex does; (c) if P-8 shows it is common or harmful.

**O5. `win_keys` pads only a `send`** (§2.7). mvp §2.7 pads both send and attach paste.
- Options: (a) `send` only (adopted); (b) both, as mvp states.
- Gains / losses: (a) an attach paste inserts exactly the pasted text, but one starting with `/`, `!`, or `?` into an empty composer triggers Codex's composer, as the same keys typed by hand do; (b) disarms those triggers but inserts spaces into a composer the human is editing (`edit ` + `foo.rs` → `edit  foo.rs`). The spike's reason for the padding is submission only (spike §4.4; arch §5).
- Recommendation: (a). Changes mvp §2.7's `win_keys` paragraph.

Fixture obligations handed on (not owner items): Claude's captures and its handling of DEL, C1, and a leading `/`, `!`, `#` (PR 13); the positional parser checks and expected form (PR 14); real delivery of every-BMP-scalar records, DEL and C1 as keys, one-part writes, and the three-paste framing (PR 15, PR 23).

## Related Text in Other Documents

- `docs/mvp-design.md` §2.3 lists the `rejected` causes as "ESC or C0 other than TAB/CR/LF … Codex-on-Windows lossy text or empty prompt": O1 adds DEL and C1 for Codex's bracketed paste (`lossy_text`), O2 makes a whitespace-only send `empty_prompt` on every row, and §2.5 rule 3 limits the empty rule for attach pastes to an empty paste.
- `docs/mvp-design.md` §2.7: the "Text encoding" row for Codex macOS gains O3's three-paste send; the `win_keys` paragraph's padding becomes send-only (O5); "the same text encoding applies to `send` and to recognised attach pastes" holds for the character encoding (D1); "every escape sequence in one write" holds as one text part (D2); the positional first prompt goes after `--` (D5).
- `docs/mvp-design.md` §7 "`cargo-fuzz`" row lists no encoder target; PR 22's targets are in §4 above.
- `docs/prior-art.md` row 4b ("rejection of ESC and C0 controls … except TAB, CR, and LF"): O1 extends the set for Codex's paste path. Rows 4a and 4c state the settled decisions this design implements: a bracketed paste waits for DECSET 2004 with the input `queued` and zero bytes written, and the newline inside the brackets is `cr`.
- `docs/design/pty-runtime.md` §5: `max_unit_bytes` (36 × + 64) stays above PR 5's bound (36 × + 39); `crates/pty-runtime/src/limits.rs` may take `MAX_EXPANSION` from `core` instead of its own `KEYED_EXPANSION`.
