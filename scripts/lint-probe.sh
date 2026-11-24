#!/usr/bin/env bash
# Checks that the timer bans of crates/daemon/clippy.toml fire (docs/design/deadlines.md §3 D4):
# clippy on the `lint_probe` test target, the negative fixture holding one call of each banned
# function, must fail with exactly one disallowed-method error per call and no other error.
#
#   scripts/lint-probe.sh                 run clippy on the probe and check its output
#   scripts/lint-probe.sh --check STATUS  check clippy output read from stdin, as if clippy had
#                                         exited with STATUS (for testing this script)
#
# Short, uncoloured output gives one line per diagnostic, primary span first, on both OSes.
set -uo pipefail

names=(timeout timeout_at sleep sleep_until)
probe='crates/daemon/tests/lint_probe\.rs'

fail() {
  echo "lint-probe: $1" >&2
  exit 1
}

check() {
  local status=$1 text spanned spanless n name
  # Windows: CRLF line ends, backslash separators, drive-absolute paths.
  text="$(tr -d '\r' | sed 's#\\#/#g')"
  [ "$status" -ne 0 ] || fail "clippy passed on the probe: the bans do not fire"
  spanned="$(grep -E '^.+:[0-9]+:[0-9]+: error(\[[^]]*\])?:' <<<"$text")"
  # The only error without a span allowed is cargo's termination summary.
  spanless="$(grep -E '^error(\[[^]]*\])?:' <<<"$text" | grep -Ev '^error: could not compile ')"
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
out="$(cargo clippy --locked -p chaldea-daemon --features lint-probe --test lint_probe \
  --color never --message-format short -- -D warnings 2>&1)"
status=$?
check "$status" <<<"$out"
