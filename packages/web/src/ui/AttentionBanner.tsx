// The attention banner (docs/design/w2-composer.md §2.4): a `<section aria-label="Attention">`,
// not a live region, listing attention.ts's entries with the actions that match each one's
// current input and state. Approvals belong to T2: `awaiting_approval` has no action.

import { hintLaunch, readyLaunch, slots, stopLaunch } from "../commands/controls";
import { hintText } from "../commands/messages";
import { busy, type CmdState } from "../commands/reducer";
import { HOST, type State } from "../sync/state";
import { attentionEntries, type Entry } from "./attention";
import { CommandLine } from "./CommandLine";
import { InputActions } from "./Inputs";
import type { Ops } from "./ops";
import { useLaunchConfirm } from "./ThreadActions";

const TITLE: Record<Entry["kind"], string> = {
  session_start_timeout: "The CLI did not start its session in time",
  startup_stall: "The CLI stalled while starting",
  input_unconfirmed: "An input may or may not have reached the CLI",
  input_gate_timeout: "A queued input waited too long for readiness",
  resume_blocked: "Resume is blocked",
  awaiting_approval: "Awaiting an approval in the CLI",
};

function ThreadEntries(props: { state: State; cmds: CmdState; thread: string; entries: Entry[]; ops: Ops }) {
  const { state, cmds, thread, entries, ops } = props;
  const { ask, dialogs } = useLaunchConfirm(state, thread, ops);
  const hint = hintLaunch(state, thread);
  const host = state.streams[HOST]?.copy;
  const info = host?.kind === "host" ? host.threads[thread]?.info : undefined;
  return (
    <li data-attention-thread={thread} className="py-1">
      <p className="text-sm">
        <span className="font-semibold">{info?.agent ?? "thread"}</span>{" "}
        <span className="break-all" dir="auto">
          {info?.cwd ?? thread}
        </span>
      </p>
      <ul>
        {entries.map((e) => (
          <li key={`${e.kind}:${e.input?.id ?? ""}`} data-cause={e.kind} className="flex flex-wrap items-center gap-2 py-1">
            <span>{TITLE[e.kind]}</span>
            {e.input !== null && (
              <>
                <InputActions state={state} cmds={cmds} thread={thread} input={e.input} ops={ops} />
                <CommandLine state={state} cmds={cmds} slot={slots.input(e.input.id)} ops={ops} />
              </>
            )}
            {e.kind === "input_gate_timeout" && (
              <button
                type="button"
                className="control"
                disabled={readyLaunch(state, thread) === null || busy(cmds, slots.ready(thread))}
                onClick={() => ask("ready")}
              >
                Mark ready
              </button>
            )}
            {(e.kind === "session_start_timeout" || e.kind === "startup_stall") && (
              <button
                type="button"
                className="control"
                disabled={stopLaunch(state, thread) === null || busy(cmds, slots.launch(thread))}
                onClick={() => ask("stop")}
              >
                Stop
              </button>
            )}
            {e.kind === "resume_blocked" && hint !== null && (
              // Only for a current launch whose death is unconfirmed.
              <span className="hint break-words text-sm">{hintText(thread, hint)}</span>
            )}
          </li>
        ))}
      </ul>
      {dialogs}
    </li>
  );
}

export function AttentionBanner(props: { state: State; cmds: CmdState; ops: Ops }) {
  const entries = attentionEntries(props.state);
  if (entries.length === 0) return null;
  const threads = [...new Set(entries.map((e) => e.thread))];
  return (
    <section aria-label="Attention" className="attention border-b border-current px-3 py-2">
      <h2 className="font-semibold">Attention</h2>
      <ul>
        {threads.map((t) => (
          <ThreadEntries
            key={t}
            state={props.state}
            cmds={props.cmds}
            thread={t}
            entries={entries.filter((e) => e.thread === t)}
            ops={props.ops}
          />
        ))}
      </ul>
    </section>
  );
}
