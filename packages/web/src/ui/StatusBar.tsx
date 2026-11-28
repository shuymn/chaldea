// The page's status: connecting, live, updates paused, or offline, plus "last synced"
// (docs/design/w1-pwa.md §2.3 "Status"). One `role="status"` region announces new-event
// counts and "back online" (debounced), and each command outcome and refused send at once
// (docs/design/w2-composer.md §2.2); going offline is a `role="alert"`. The latest command
// outcome also stays visible here, naming its thread, whatever is selected.

import { useEffect, useRef, useState } from "react";

import { noticeText } from "../commands/messages";
import { type CmdState, initialCmdState } from "../commands/reducer";
import { HOST, type State, status } from "../sync/state";

const ANNOUNCE_MS = 1000;
const time = new Intl.DateTimeFormat(undefined, { timeStyle: "medium" });
const NO_COMMANDS = initialCmdState();

/** A thread as the status bar names it: agent and cwd from the host copy, else its id. */
export function threadLabel(state: State, thread: string): string {
  const host = state.streams[HOST]?.copy;
  const info = host?.kind === "host" ? host.threads[thread]?.info : undefined;
  return info ? `${info.agent} ${info.cwd}` : `thread ${thread}`;
}

export function StatusBar(props: { state: State; cmds?: CmdState }) {
  const { state } = props;
  const cmds = props.cmds ?? NO_COMMANDS;
  const now = status(state);
  const [message, setMessage] = useState("");
  const counted = useRef(state.appended);
  const was = useRef(now);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const notice = cmds.notice;
  const last = notice === null ? null : noticeText(cmds, notice, (t) => threadLabel(state, t));

  useEffect(() => {
    const backOnline = was.current === "offline" && now !== "offline";
    was.current = now;
    const fresh = state.appended - counted.current;
    if (!backOnline && fresh <= 0) return;
    if (timer.current !== null) clearTimeout(timer.current);
    timer.current = setTimeout(() => {
      timer.current = null;
      const n = state.appended - counted.current;
      counted.current = state.appended;
      const parts = [];
      if (backOnline) parts.push("Back online.");
      if (n > 0) parts.push(n === 1 ? "1 new event." : `${n} new events.`);
      setMessage(parts.join(" "));
    }, ANNOUNCE_MS);
  }, [now, state.appended]);

  // Each new notice is announced once, when it arrives.
  const seq = notice?.seq ?? 0;
  const announced = useRef(seq);
  useEffect(() => {
    if (seq === announced.current) return;
    announced.current = seq;
    if (last !== null) setMessage(last);
  }, [seq, last]);

  useEffect(
    () => () => {
      if (timer.current !== null) clearTimeout(timer.current);
    },
    [],
  );

  return (
    <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 text-sm">
      <span data-status={now} className="font-semibold">
        {now}
      </span>
      <span>
        {state.lastSyncedAt === null
          ? "not synced yet"
          : `last synced ${time.format(new Date(state.lastSyncedAt))}`}
      </span>
      {last !== null && (
        <span data-last-command className="min-w-0 break-all" dir="auto">
          {last}
        </span>
      )}
      <span role="status" className="sr-only">
        {message}
      </span>
      {now === "offline" && (
        <span role="alert" className="font-semibold">
          Offline: the daemon is unreachable. Showing the last synced state.
        </span>
      )}
    </div>
  );
}
