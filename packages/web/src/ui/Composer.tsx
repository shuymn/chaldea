// The composer of the selected thread (docs/design/w2-composer.md §2.1): a `<form>` with a
// visible label, a plain `<textarea>` (invariant 4), and a submit button. Enter follows
// keys.ts; the composing flag and the last `compositionend` reset on blur and on a thread
// switch (the parent keys this component by thread). The draft lives in the command state.

import { useId, useRef } from "react";

import { canSend, lockReason, slots } from "../commands/controls";
import { INVALID_TEXT } from "../commands/messages";
import { busy, type CmdState, draftOf } from "../commands/reducer";
import type { State } from "../sync/state";
import { CommandLine } from "./CommandLine";
import { decide } from "./keys";
import type { Ops } from "./ops";

const finePointer = () => typeof matchMedia === "function" && matchMedia("(pointer: fine)").matches;

export function Composer(props: { state: State; cmds: CmdState; thread: string; ops: Ops }) {
  const { state, cmds, thread, ops } = props;
  const id = useId();
  const composing = useRef(false);
  const lastEnd = useRef<number | null>(null);
  const form = useRef<HTMLFormElement>(null);
  const draft = draftOf(cmds, thread);
  const invalid = cmds.invalid[thread];
  const slot = slots.send(thread);
  const lock = lockReason(state, thread);
  const sendable = canSend(state, thread) && !busy(cmds, slot);
  const reset = () => {
    composing.current = false;
    lastEnd.current = null;
  };
  return (
    <form
      ref={form}
      className="composer flex flex-col gap-2 px-3 pb-3"
      aria-label="Composer"
      onSubmit={(e) => {
        e.preventDefault();
        // Ctrl+Enter submits even with the button disabled: a held slot or a lock is refused
        // in the reducer, which posts nothing and says why (a notice).
        ops.send(thread);
      }}
    >
      <label id={`${id}-label`} htmlFor={`${id}-text`} className="font-semibold">
        Message
      </label>
      <textarea
        id={`${id}-text`}
        className="composer-text"
        rows={2}
        value={draft.text}
        aria-invalid={invalid !== undefined ? true : undefined}
        aria-describedby={`${id}-hint${invalid !== undefined ? ` ${id}-error` : ""}`}
        onChange={(e) => ops.edit(thread, e.target.value)}
        onCompositionStart={() => {
          composing.current = true;
        }}
        onCompositionEnd={(e) => {
          composing.current = false;
          lastEnd.current = e.nativeEvent.timeStamp;
        }}
        onBlur={reset}
        onKeyDown={(e) => {
          const n = e.nativeEvent;
          const d = decide({
            key: n.key,
            repeat: n.repeat,
            shift: n.shiftKey,
            ctrlOrMeta: n.ctrlKey || n.metaKey,
            isComposing: n.isComposing,
            keyCode: n.keyCode,
            composingFlag: composing.current,
            timeStamp: n.timeStamp,
            lastCompositionEnd: lastEnd.current,
            finePointer: finePointer(),
          });
          if (d === "submit") {
            e.preventDefault();
            form.current?.requestSubmit();
          } else if (d === "ignore") {
            e.preventDefault();
          }
        }}
      />
      <p id={`${id}-hint`} className="text-sm">
        {lock ?? "Enter sends on a desktop pointer, Ctrl or Cmd+Enter anywhere; Shift+Enter adds a line."}
      </p>
      {invalid !== undefined && (
        <p id={`${id}-error`} className="text-sm font-semibold">
          {INVALID_TEXT[invalid]}
        </p>
      )}
      <div className="flex flex-wrap items-center gap-2">
        <button type="submit" className="control" disabled={!sendable}>
          Send
        </button>
      </div>
      <CommandLine state={state} cmds={cmds} slot={slot} ops={ops} showText />
    </form>
  );
}
