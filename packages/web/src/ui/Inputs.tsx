// The selected thread's inputs (docs/design/w2-composer.md §2.4): non-terminal ones with
// their actions, then the terminal transitions this copy observed with reason and actor.
// State is the stream's (D7): a reply never changes what this list shows.

import { canCancel, canResolve, slots } from "../commands/controls";
import { rejectText } from "../commands/messages";
import { busy, type CmdState } from "../commands/reducer";
import type { InputView, State } from "../sync/state";
import { threadCopy } from "../sync/state";
import { CommandLine } from "./CommandLine";
import type { Ops } from "./ops";

const SHOWN: Record<InputView["state"], string> = {
  queued: "waiting for readiness",
  sending: "in flight",
  sent: "in flight",
  unconfirmed: "may have reached the CLI",
  acked: "acked",
  rejected: "rejected",
  cancelled: "cancelled",
  dismissed: "dismissed",
};

export function reasonText(i: InputView): string | null {
  const r = i.reason;
  if (r === null) return null;
  if (r.kind === "rejected") return rejectText(r.reject);
  return r.kind.replaceAll("_", " ");
}

function preview(text: string): string {
  return text.length > 120 ? `${text.slice(0, 120)}…` : text;
}

export function InputActions(props: { state: State; cmds: CmdState; thread: string; input: InputView; ops: Ops }) {
  const { state, cmds, thread, input, ops } = props;
  const slot = slots.input(input.id);
  const held = busy(cmds, slot);
  if (input.state === "queued") {
    return (
      <button
        type="button"
        className="control"
        disabled={held || !canCancel(state, thread, input.id)}
        onClick={() => ops.cancel(thread, input.id)}
      >
        Cancel
      </button>
    );
  }
  if (input.state === "unconfirmed") {
    const ok = !held && canResolve(state, thread, input.id);
    return (
      <>
        <button type="button" className="control" disabled={!ok} onClick={() => ops.resolve(thread, input.id, true)}>
          It arrived
        </button>
        <button type="button" className="control" disabled={!ok} onClick={() => ops.resolve(thread, input.id, false)}>
          Dismiss
        </button>
      </>
    );
  }
  return null;
}

export function Inputs(props: { state: State; cmds: CmdState; thread: string; ops: Ops }) {
  const { state, cmds, thread, ops } = props;
  const copy = threadCopy(state, thread);
  if (copy === null || (copy.inputs.length === 0 && copy.done.length === 0)) return null;
  const rows = [...copy.inputs, ...[...copy.done].reverse()];
  return (
    <section aria-labelledby="inputs-heading" className="px-3 pb-2">
      <h3 id="inputs-heading" className="font-semibold">
        Inputs
      </h3>
      <ul>
        {rows.map((i) => {
          const reason = reasonText(i);
          return (
            <li key={`${i.id}:${i.state}`} data-input={i.id} data-state={i.state} className="border-b border-neutral-300 py-1 dark:border-neutral-700">
              <p className="flex flex-wrap items-center gap-x-2 text-sm">
                <span className="font-semibold">{SHOWN[i.state]}</span>
                {reason !== null && <span>({reason})</span>}
                {i.by !== null && <span>by {i.by}</span>}
                <span className="break-all">from {i.createdBy}</span>
              </p>
              <p className="whitespace-pre-wrap break-all text-sm" dir="auto">
                {preview(i.preview)}
              </p>
              <div className="flex flex-wrap items-center gap-2">
                <InputActions state={state} cmds={cmds} thread={thread} input={i} ops={ops} />
              </div>
              <CommandLine state={state} cmds={cmds} slot={slots.input(i.id)} ops={ops} />
            </li>
          );
        })}
      </ul>
    </section>
  );
}
