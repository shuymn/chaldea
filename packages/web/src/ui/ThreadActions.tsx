// The selected thread's launch controls (docs/design/w2-composer.md §2.2): Mark ready and
// Stop confirm in a dialog that pins the launch (a launch change closes it), Resume, the
// hint for an exited launch whose death is not confirmed, the thread's `raw_unknown` count,
// and the results of its ready and stop/resume slots.

import { useEffect, useState } from "react";

import { hintLaunch, readyLaunch, resumeLaunch, slots, stopLaunch } from "../commands/controls";
import { hintText } from "../commands/messages";
import { busy, type CmdState } from "../commands/reducer";
import { type State, threadCopy } from "../sync/state";
import { CommandLine, ConfirmDialog } from "./CommandLine";
import type { Ops } from "./ops";

type Pin = { readonly kind: "ready" | "stop"; readonly launch: string };

/**
 * The Ready and Stop confirmations for one thread: `ask` opens one pinned to the launch
 * available now; the dialog closes when that launch stops being the one available.
 */
export function useLaunchConfirm(state: State, thread: string, ops: Ops) {
  const [pin, setPin] = useState<Pin | null>(null);
  const avail = pin === null ? null : pin.kind === "ready" ? readyLaunch(state, thread) : stopLaunch(state, thread);
  const stale = pin !== null && avail !== pin.launch;
  useEffect(() => {
    if (stale) setPin(null);
  }, [stale]);
  const ask = (kind: Pin["kind"]) => {
    const launch = kind === "ready" ? readyLaunch(state, thread) : stopLaunch(state, thread);
    if (launch !== null) setPin({ kind, launch });
  };
  const close = () => setPin(null);
  const dialogs = (
    <>
      <ConfirmDialog
        open={pin?.kind === "ready" && !stale}
        title="Mark the CLI ready?"
        confirm="Mark ready"
        onClose={close}
        onConfirm={() => pin && ops.ready(thread, pin.launch)}
      >
        <p>
          You assert that this launch's CLI shows its normal composer, open and empty, with no popup, menu, or search
          open. Queued prompts may then be written to it.
        </p>
        <p className="mt-1 break-all">Launch {pin?.launch}</p>
      </ConfirmDialog>
      <ConfirmDialog
        open={pin?.kind === "stop" && !stale}
        title="Stop this launch?"
        confirm="Stop"
        onClose={close}
        onConfirm={() => pin && ops.stop(thread, pin.launch)}
      >
        <p>The CLI and its process tree are stopped. Queued inputs are cancelled.</p>
        <p className="mt-1 break-all">Launch {pin?.launch}</p>
      </ConfirmDialog>
    </>
  );
  return { ask, dialogs };
}

export function ThreadActions(props: { state: State; cmds: CmdState; thread: string; ops: Ops }) {
  const { state, cmds, thread, ops } = props;
  const { ask, dialogs } = useLaunchConfirm(state, thread, ops);
  const copy = threadCopy(state, thread);
  const ready = readyLaunch(state, thread);
  const stop = stopLaunch(state, thread);
  const resume = resumeLaunch(state, thread);
  const hint = hintLaunch(state, thread);
  return (
    <section aria-label="Thread actions" className="flex flex-col gap-2 px-3 pb-2">
      <div className="flex flex-wrap items-center gap-2">
        <button type="button" className="control" disabled={ready === null || busy(cmds, slots.ready(thread))} onClick={() => ask("ready")}>
          Mark ready
        </button>
        <button type="button" className="control" disabled={stop === null || busy(cmds, slots.launch(thread))} onClick={() => ask("stop")}>
          Stop
        </button>
        {hint === null && (
          <button
            type="button"
            className="control"
            disabled={resume === null || busy(cmds, slots.launch(thread))}
            onClick={() => resume !== null && ops.resume(thread, resume)}
          >
            Resume
          </button>
        )}
        {copy !== null && <span className="text-sm">raw_unknown count: {copy.rawUnknown}</span>}
      </div>
      {hint !== null && <p className="hint break-words text-sm">{hintText(thread, hint)}</p>}
      <CommandLine state={state} cmds={cmds} slot={slots.ready(thread)} ops={ops} />
      <CommandLine state={state} cmds={cmds} slot={slots.launch(thread)} ops={ops} />
      {dialogs}
    </section>
  );
}
