// -*- mode: Bluespec; -*-
/**
 * Helpers the Q-B modules share (docs/design/formal-specs.md §7).
 */
module common {
  /// `m` without the key `k`.
  pure def without(m: a -> b, k: a): a -> b = m.keys().exclude(Set(k)).mapBy(x => m.get(x))

  /// The candidates a simulation step picks from: `ids`, or `Set(0)` (no such id) when it is
  /// empty, so that `oneOf` stays defined and the step's guard fails.
  pure def orNone(ids: Set[int]): Set[int] = if (ids.size() == 0) Set(0) else ids
}
