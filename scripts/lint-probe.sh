#!/usr/bin/env bash
# Checks that the timer bans of crates/daemon/clippy.toml fire (docs/design/deadlines.md §3 D4).
# Clippy runs on the `lint_probe` test target, the negative fixture holding one call of each
# banned function, and the verdict comes from cargo's JSON messages on stdout only: cargo
# exits 101; the build has exactly four error-level compiler messages (internal compiler errors
# included), each in the `lint_probe` target, `clippy::disallowed_methods`, with its primary
# span in the fixture, one per banned function; and cargo reports the build finished without
# success. A compiler child killed after printing all four messages is reported only on stderr
# and is outside the probe's coverage.
#
#   scripts/lint-probe.sh                 run clippy on the probe and check its messages
#   scripts/lint-probe.sh --check STATUS  check JSON messages read from stdin, as if cargo had
#                                         exited with STATUS (for testing this script)
#
# Needs jq (pinned in mise.toml): run it as `mise exec jq -- bash scripts/lint-probe.sh`.
# `--workspace` with the feature named through the package resolves the same dependency
# features as the workspace clippy step, so after that step it reuses its check artifacts.
set -uo pipefail

fail() {
  echo "lint-probe: $1" >&2
  exit 1
}

# The banned function is read from the message, which names the resolved path whatever the
# call's spelling; the span's text would show only `sleep_until` for an imported call.
# The verdict is one JSON-escaped string printed without a newline (`-j`), so no CR appears on
# Windows.
verdict='
def norm: gsub("\\\\"; "/");
def banned: ["tokio::time::sleep", "tokio::time::sleep_until", "tokio::time::timeout", "tokio::time::timeout_at"];
def primary_spans: [.spans[] | select(.is_primary)];
[.[] | select(.reason == "compiler-message" and (.message.level | startswith("error")))] as $recs
| [$recs[] | .message] as $errs
| [.[] | select(.reason == "build-finished")] as $done
| if ($done | length) != 1 or $done[0].success != false then
    "expected one build-finished record without success, found \($done)"
  elif ($errs | length) != (banned | length) then
    "expected \(banned | length) errors, found \($errs | length): \([$errs[].message])"
  elif any($recs[]; .target.name != "lint_probe") then
    "an error in a target other than lint_probe: \([$recs[] | .target.name])"
  elif any($errs[]; .code.code? != "clippy::disallowed_methods") then
    "an error other than clippy::disallowed_methods: \([$errs[] | select(.code.code? != "clippy::disallowed_methods") | .message])"
  elif any($errs[]; primary_spans as $p
      | ($p | length) != 1 or ($p[0].file_name | norm | test("(^|/)crates/daemon/tests/lint_probe\\.rs$") | not)) then
    "an error whose primary span is not in the probe: \([$errs[] | [primary_spans[] | .file_name]])"
  elif ([$errs[] | .message | capture("`(?<p>tokio::time::[a-z_]+)`").p] | sort) != banned then
    "expected one error per banned function, found \([$errs[].message])"
  else
    "ok"
  end
'

check() {
  local status=$1 v
  command -v jq >/dev/null || fail "jq not found; run through mise exec (mise.toml pins it)"
  [ "$status" -eq 101 ] || fail "cargo exited $status, not 101 (a failed build)"
  v="$(jq -js "$verdict")" || fail "cargo's output is not JSON messages"
  [ "$v" = ok ] || fail "$v"
  echo "lint-probe: all 4 bans fire"
}

if [ "${1:-}" = "--check" ]; then
  check "${2:?STATUS}"
  exit
fi

cd "$(dirname "$0")/.." || exit 1
out="$(cargo clippy --locked --workspace --features chaldea-daemon/lint-probe --test lint_probe \
  --message-format json -- -D warnings 2>/dev/null)"
status=$?
check "$status" <<<"$out"
