// The attention banner's entries (docs/design/w2-composer.md §2.4): the causes and the
// `awaiting_approval` turn state of followed threads whose own copy the page holds (F1),
// never a host "last known" value. They follow the copy: `status_changed`, snapshots, and
// unfollowing (which drops the copy).

import type { AttentionCause } from "@chaldea/contracts/generated/AttentionCause";

import { HOST, type InputView, type State, threadCopy, threadOf } from "../sync/state";

export type Entry =
  | { readonly thread: string; readonly kind: AttentionCause["kind"]; readonly input: InputView | null }
  | { readonly thread: string; readonly kind: "awaiting_approval"; readonly input: null };

export function attentionEntries(s: State): Entry[] {
  const out: Entry[] = [];
  for (const key of s.followed) {
    if (key === HOST) continue;
    const thread = threadOf(key);
    if (thread === null) continue;
    const copy = threadCopy(s, thread);
    if (copy === null) continue;
    for (const cause of copy.attention) {
      // The actions act on the input the cause names, in the state the cause needs.
      const id = "input_id" in cause ? cause.input_id : null;
      const want = cause.kind === "input_unconfirmed" ? "unconfirmed" : "queued";
      const input = id === null ? null : (copy.inputs.find((i) => i.id === id && i.state === want) ?? null);
      out.push({ thread, kind: cause.kind, input });
    }
    if (copy.turn === "awaiting_approval") out.push({ thread, kind: "awaiting_approval", input: null });
  }
  return out;
}
