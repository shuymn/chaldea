# Claude Code 2.1.331 hook and transcript fixtures

Captured on macOS from a real `claude` 2.1.331 launched by the daemon with its
`claude-settings.json` in place and no decoder (`NoDecoder`), so every hook was kept as
`raw_unknown` with its exact bytes. Each `<Event>-<n>.json` is one hook body exported from
`GET /api/events`, numbered per event in receipt order within its case. Paths are scrubbed:
the workspace is `/work/c1b-ws`, the home directory `/work/profile`, Claude's temporary
directory `/work/tmp/claude/`. The capture folder's `.claude/settings.json` set
`permissions.defaultMode` to `default`, so tool calls asked for permission.

`corpus-wrapped-small` was captured later the same way, except that the daemon decoded Claude's
hooks: its `UserPromptSubmit` body is the stdin of an extra command hook the capture folder's
settings added, which saved it to a file; the daemon recorded the same prompt.

Each `<Event>-<n>.meta.json` holds, from the captures and independent of the decoder: `fact`
(what the hook says), `events` (the adapter's own events, `tool_activity` here), `route`
(`bind`, `interpret`, or `foreign_session`, as the daemon routed it), and for a
`UserPromptSubmit`, `sent` (the text the daemon sent, `null` for a prompt Claude generated) and
`expected_prompt` (the recorded prompt, literally).

`<case>/transcript.jsonl` is an excerpt of the session transcript Claude wrote during that case
(`~/.claude/projects/<cwd>/<session>.jsonl`): selected whole lines, in the file's order, scrubbed
like the hooks (also the emitter path, now `/work/bin/chaldea`, and each thinking `signature`,
now `scrubbed`), and written back as compact JSON. Lines that carry the capture host's own
setup (skill, tool, and instruction listings, the system prompt in `api-request-shape` and most
`api-request-blob` lines) are left out. `transcript.meta.json` holds the number of lines and the
events the tailer stores from them, from the captures: one `assistant_message` per `assistant`
line with a `text` block (the text blocks concatenated, `occurred_at` its `timestamp`); every
other line is a known type that stores nothing.

| Case | Line types in the excerpt |
|---|---|
| `plain-turn` | `mode`, `permission-mode`, `atis-latch`, `file-history-snapshot`, `user`, `attachment`, `api-request`, `last-prompt`, `assistant` (text), `system`, `ai-title`, `cost-state` |
| `permission-approved` | `file-history-snapshot`, `user` (prompt, `tool_result`), `api-request`, `api-request-blob`, `last-prompt`, `tools-used`, `assistant` (`tool_use`, text), `ai-title`, `system` |
| `subagent` | `ai-title`, `tools-used`, `assistant` (`tool_use`, thinking, text), `user` (`tool_result`, meta, `<task-notification>`), `system`, `file-history-snapshot`, `queue-operation` |
| `corpus-wrapped-small` | `data-flow-records`, `atis-latch`, `ai-title`, `assistant` (thinking, text), `data-flow-incomplete`, `system`, `api-request` |

## Cases

| Case | What was done | Hooks, in receipt order |
|---|---|---|
| `untrusted-folder` | spawn in a folder never trusted; the trust dialog answered "Yes" | nothing until the answer; `SessionStart` (`startup`) about 110 ms after it |
| `plain-turn` | spawn in the trusted folder, one prompt | `SessionStart` (`startup`), `UserPromptSubmit`, `Stop` |
| `corpus-*` | one corpus prompt each (below) | `UserPromptSubmit`, `Stop`; `corpus-bang`: `Stop` only; `corpus-wrapped-small`: `UserPromptSubmit` only (below) |
| `permission-approved` | a Bash call, the dialog answered with Enter | `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `PostToolUse`, `Stop` |
| `permission-denied` | a Bash call, the dialog answered with Escape | `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, then nothing |
| `subagent` | an Agent tool call whose subagent ran Bash (approved) | `UserPromptSubmit`, `PreToolUse` and `PostToolUse` (Agent), `Stop`, then the subagent's `PreToolUse`, `PermissionRequest`, `PostToolUse` (Bash), then `UserPromptSubmit` (`<task-notification>`), `Stop` |
| `compact` | `/compact` sent | `SessionStart` (`compact`, same session) |
| `clear` | `/clear` sent, then a prompt | `SessionStart` (`clear`, new session), `UserPromptSubmit`, `Stop`, all with the new session id |
| `resume-in-session` | `/resume` sent, another session picked, then a prompt | `SessionStart` (`resume`, the picked session), `UserPromptSubmit`, `Stop` with that id |
| `resume` | the launch stopped, `chaldea resume` (`--resume <session>`), then a prompt | `SessionStart` (`resume`, same session), `UserPromptSubmit`, `Stop` |

## Payload fields

Every hook carries `session_id`, `transcript_path`, `cwd`, `hook_event_name`.

| Event | Other fields seen |
|---|---|
| `SessionStart` | `source` (`startup`, `resume`, `clear`, `compact`); `model` (not on a launch's `resume`); `prompt_id` only for `compact` and an in-session `resume`; on `resume`: `context_tokens`, `estimated_cache_write_usd`, `prompt_cache_likely_expired`, `seconds_since_last_response` |
| `UserPromptSubmit` | `prompt_id`, `permission_mode`, `prompt` |
| `Stop` | `prompt_id`, `permission_mode`, `effort`, `stop_hook_active`, `last_assistant_message`, `background_tasks`, `session_crons` |
| `PreToolUse` | `prompt_id`, `permission_mode`, `effort`, `tool_name`, `tool_input`, `tool_use_id`; a subagent's also `agent_id`, `agent_type` |
| `PermissionRequest` | `prompt_id`, `permission_mode`, `effort`, `tool_name`, `tool_input`, `permission_suggestions`; a subagent's also `agent_id`, `agent_type`; **no `tool_use_id`** |
| `PostToolUse` | `prompt_id`, `permission_mode`, `effort`, `tool_name`, `tool_input`, `tool_response`, `tool_use_id`, `duration_ms`; a subagent's also `agent_id`, `agent_type` |

## Findings

- **`PermissionRequest` against `PreToolUse`**: the call's `PreToolUse` comes first (about 25 ms
  before here), with the same `tool_name` and `tool_input`; the `PermissionRequest` has no
  `tool_use_id`, so a card is correlated only by `tool_name` and `tool_input`.
- **`prompt_id`**: present on every `UserPromptSubmit`, `Stop`, `PreToolUse`,
  `PermissionRequest`, and `PostToolUse`, equal across one turn's hooks (a subagent's hooks carry
  the turn that started it); absent on `SessionStart` except `compact` and an in-session
  `resume`.
- **`Stop` continuation**: `stop_hook_active` was `false` in every capture (no hook blocks a
  stop).
- **Settle after `Stop`**: a prompt typed about 20 ms after the `Stop` was seen in the store
  (about 90 ms after its commit) was recorded intact as the next `UserPromptSubmit`; no longer
  settle was needed here, so the daemon's 2 s settle has margin.
- **Post-decision signal**: an approval is followed by the call's `PostToolUse` (80–130 ms
  after the key's write was recorded); a denial with Escape interrupts the turn, and no hook follows (no
  `PostToolUse`, no `Stop`). Nothing else tells a decision.
- **Hook ancestry**: the hook command runs under `/bin/sh`, whose parent is the launch's
  `claude` process (the PTY child), whose parent is the daemon.
- **Trust prompt**: no hook arrives while the trust dialog is shown. Its default choice is
  "No, exit": Enter alone ends `claude` with code 1 and no hook.
- **Session switches**: `/clear` and an in-session `/resume` start another session id, and every
  later hook carries it; `/compact` keeps the session id; a launch's `--resume <session>` keeps
  it.
- **Subagents**: the Agent tool ran asynchronously (`tool_response.isAsync`): its `PostToolUse`
  and the turn's `Stop` (with the subagent in `background_tasks`) came before the subagent's own
  hooks, and its completion arrived as a `UserPromptSubmit` whose `prompt` is a
  `<task-notification>` that no input sent, starting a turn of its own.

## Prompt corpus

Each prompt was sent by `chaldea send` as one bracketed paste (DECSET 2004 on).

| Case | Sent | Recorded |
|---|---|---|
| `corpus-fa19` | U+FA19 | U+795E: NFC of the whole prompt |
| `corpus-ends` | LF, two spaces, `both ends`, two spaces, LF | LF, two spaces, `both ends`: trailing whitespace removed, leading kept |
| `corpus-tab` | `a` TAB `b` | `a`, four spaces, `b` |
| `corpus-wrapped` | 300 lines of `e` U+0301 TAB `か` U+3099 ` line` LF (4800 bytes) | the large-paste wrapper around the lines with each TAB as four spaces, the marks **not** composed, and the last LF removed |
| `corpus-wrapped-small` | 801 `y` | the wrapper around the 801 `y`s |
| `corpus-del` | `a` DEL `b` | `ab` |
| `corpus-c1` | `a` U+0080 `b` U+009F `c` | `abc` |
| `/zzexample corpus` (no files) | leading `/` | no hook: Claude ran it as an unknown command |
| `corpus-bang` | `!echo corpus` | no `UserPromptSubmit`: Claude ran it as a shell command, then a `Stop` |
| `corpus-hash` | `#zz corpus` | unchanged |

So a prompt is recorded as `tab4(nfc(trim_end(remove DEL and C1(lf(t)))))`, or, when Claude
folds the paste, as the wrapper around `tab4(trim_end(lf(t)))`. Claude folds a paste of more than
800 UTF-16 code units or of more newlines than `max(0, min(rows − 10, 2))` (its paste handler), so an 801-byte
paste is wrapped too; a three-line paste (two newlines) was not. Spike C-1 (2.1.326, Windows) saw
decomposed entries composed, which agrees with the first form.
