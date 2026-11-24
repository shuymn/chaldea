#!/usr/bin/env bash
# Checks that the timer bans of crates/daemon/clippy.toml fire (docs/design/deadlines.md §3 D4):
# clippy on the `lint_probe` test target, the negative fixture holding one call of each banned
# function, must fail with exactly one disallowed-method error per call, cargo's normal summary
# for that target, and nothing else that reports an error or an abnormal exit.
#
#   scripts/lint-probe.sh                 run clippy on the probe and check its output
#   scripts/lint-probe.sh --check STATUS  check clippy output read from stdin, as if clippy had
#                                         exited with STATUS (for testing this script)
#
# Short, uncoloured output gives one line per diagnostic, primary span first, on both OSes.
# The selection (`--workspace` with the feature named through the package) resolves the same
# dependency features as the workspace clippy step, so the probe reuses its check artifacts.
set -uo pipefail

names=(timeout timeout_at sleep sleep_until)
probe='crates/daemon/tests/lint_probe\.rs'
summary="^error: could not compile \`chaldea-daemon\` \\(test \"lint_probe\"\\) due to ${#names[@]} previous errors(; [0-9]+ warnings? emitted)?\$"

fail() {
  echo "lint-probe: $1" >&2
  exit 1
}

check() {
  local status=$1 text spanned spanless n name
  # Windows: CRLF line ends, backslash separators, drive-absolute paths.
  text="$(tr -d '\r' | sed 's#\\#/#g')"
  [ "$status" -ne 0 ] || fail "clippy passed on the probe: the bans do not fire"
  # A compiler that crashed or was killed after its diagnostics: cargo's trailer of an
  # abnormal exit.
  ! grep -Eiq '^Caused by:|process did(n.t| not) exit successfully|signal: [0-9]+|SIGKILL|SIGSEGV|SIGABRT|panicked at|internal compiler error' <<<"$text" ||
    fail "abnormal exit: $(grep -Ei 'Caused by:|exit successfully|signal|panicked|internal compiler error' <<<"$text" | head -3)"
  spanned="$(grep -E '^.+:[0-9]+:[0-9]+: error(\[[^]]*\])?:' <<<"$text")"
  # The only error without a span allowed is cargo's normal summary for the probe, once.
  n="$(grep -Ec "$summary" <<<"$text")"
  [ "$n" -eq 1 ] || fail "expected one normal summary for the probe target, found $n"
  spanless="$(grep -E '^error(\[[^]]*\])?:' <<<"$text" | grep -Ev "$summary")"
  [ -z "$spanless" ] || fail "unexpected error: $spanless"
  n="$(grep -c . <<<"$spanned")"
  [ "$n" -eq ${#names[@]} ] || fail "expected ${#names[@]} errors, found $n: $spanned"
  for name in "${names[@]}"; do
    n="$(grep -Ec "(^|/)${probe}:[0-9]+:[0-9]+: error: use of a disallowed method \`tokio::time::${name}\`" <<<"$spanned")"
    [ "$n" -eq 1 ] || fail "expected one disallowed-method error for tokio::time::${name} in the probe, found $n"
  done
  echo "lint-probe: all ${#names[@]} bans fire"
}

if [ "${1:-}" = "--check" ]; then
  check "${2:?STATUS}"
  exit
fi

cd "$(dirname "$0")/.." || exit 1
out="$(cargo clippy --locked --workspace --features chaldea-daemon/lint-probe --test lint_probe \
  --color never --message-format short -- -D warnings 2>&1)"
status=$?
check "$status" <<<"$out"
