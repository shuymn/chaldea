// The page's status: connecting, live, updates paused, or offline, plus "last synced"
// (docs/design/w1-pwa.md §2.3 "Status"). One `role="status"` region announces new-event
// counts and "back online" (debounced), and each command outcome and refused send at once
// (docs/design/w2-composer.md §2.2); going offline is a `role="alert"`. The latest command
// outcome also stays visible here, its text and thread frozen when it was recorded.

import { useEffect, useRef, useState } from "react";

import type { CmdState } from "../commands/reducer";
import { type State, status } from "../sync/state";

const ANNOUNCE_MS = 1000;
const time = new Intl.DateTimeFormat(undefined, { timeStyle: "medium" });

/** One announcement: `key` makes each one a new node, so an identical text announces again. */
type Message = { readonly key: string; readonly text: string };

export function StatusBar(props: { state: State; cmds?: CmdState }) {
  const { state } = props;
  const now = status(state);
  const [message, setMessage] = useState<Message>({ key: "none", text: "" });
  const counted = useRef(state.appended);
  const was = useRef(now);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const notice = props.cmds?.notice ?? null;

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
      setMessage({ key: `sync:${state.appended}:${now}`, text: parts.join(" ") });
    }, ANNOUNCE_MS);
  }, [now, state.appended]);

  // Each new notice is announced once, at once. It replaces a pending counter message.
  const seq = notice?.seq ?? 0;
  const announced = useRef(seq);
  useEffect(() => {
    if (notice === null || seq === announced.current) return;
    announced.current = seq;
    if (timer.current !== null) {
      clearTimeout(timer.current);
      timer.current = null;
      counted.current = state.appended;
    }
    setMessage({ key: `notice:${seq}`, text: notice.text });
  }, [seq, notice, state.appended]);

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
      {notice !== null && (
        <span data-last-command className="min-w-0 break-all" dir="auto">
          {notice.text}
        </span>
      )}
      <span role="status" className="sr-only">
        <span key={message.key}>{message.text}</span>
      </span>
      {now === "offline" && (
        <span role="alert" className="font-semibold">
          Offline: the daemon is unreachable. Showing the last synced state.
        </span>
      )}
    </div>
  );
}
