// Shared command pieces: one command's state and result with its Retry and Forget, and the
// confirming dialog (a native `<dialog>` opened with `showModal()`, accessibility §12).

import { useEffect, useId, useRef } from "react";

import { canRetry } from "../commands/controls";
import { busy, type CmdState } from "../commands/reducer";
import { commandText } from "../commands/messages";
import type { State } from "../sync/state";
import type { Ops } from "./ops";

export function CommandLine(props: { state: State; cmds: CmdState; slot: string; ops: Ops; showText?: boolean }) {
  const { cmds, slot, ops } = props;
  const c = cmds.slots[slot];
  if (!c) return null;
  const forgettable = c.phase === "check_thread" || c.phase === "not_done";
  return (
    <div className="command-line flex flex-wrap items-center gap-2 text-sm" data-slot={slot} data-phase={c.phase}>
      <span>{commandText(c)}</span>
      {props.showText && c.text !== null && busy(cmds, slot) && (
        <span className="whitespace-pre-wrap break-all italic" dir="auto">
          {c.text.length > 200 ? `${c.text.slice(0, 200)}…` : c.text}
        </span>
      )}
      {canRetry(cmds, props.state, slot) && (
        <button type="button" className="control" onClick={() => ops.retry(slot, c.attempt)}>
          Retry
        </button>
      )}
      {forgettable && (
        <button type="button" className="control" onClick={() => ops.forget(slot)}>
          Forget
        </button>
      )}
    </div>
  );
}

/**
 * A modal confirmation. `open` drives `showModal()` / `close()`; Escape and Cancel call
 * `onClose`. The parent closes it when the launch it pinned is no longer current.
 */
export function ConfirmDialog(props: {
  open: boolean;
  title: string;
  children: React.ReactNode;
  confirm: string;
  onConfirm: () => void;
  onClose: () => void;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  const titleId = useId();
  const { open } = props;
  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    if (open && !d.open) d.showModal?.();
    if (!open && d.open) d.close();
  }, [open]);
  return (
    <dialog
      ref={ref}
      aria-labelledby={titleId}
      className="confirm m-auto max-w-[min(32rem,calc(100vw-2rem))] rounded border border-current bg-[Canvas] p-4 text-[CanvasText]"
      onClose={props.onClose}
    >
      <h2 id={titleId} className="mb-2 text-lg font-semibold">
        {props.title}
      </h2>
      <div className="mb-3 text-sm">{props.children}</div>
      <div className="flex flex-wrap justify-end gap-2">
        <button type="button" className="control" onClick={props.onClose}>
          Cancel
        </button>
        <button
          type="button"
          className="control font-semibold"
          onClick={() => {
            props.onConfirm();
            props.onClose();
          }}
        >
          {props.confirm}
        </button>
      </div>
    </dialog>
  );
}
