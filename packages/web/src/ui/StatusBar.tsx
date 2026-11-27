// The page's status: connecting, live, updates paused, or offline, plus "last synced"
// (docs/design/w1-pwa.md §2.3 "Status"). One debounced `role="status"` region announces
// new-event counts and "back online"; going offline is a `role="alert"`.

import { useEffect, useRef, useState } from "react";

import { type State, status } from "../sync/state";

const ANNOUNCE_MS = 1000;
const time = new Intl.DateTimeFormat(undefined, { timeStyle: "medium" });

export function StatusBar(props: { state: State }) {
  const { state } = props;
  const now = status(state);
  const [message, setMessage] = useState("");
  const counted = useRef(state.appended);
  const was = useRef(now);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

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

  useEffect(
    () => () => {
      if (timer.current !== null) clearTimeout(timer.current);
    },
    [],
  );

  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm">
      <span data-status={now} className="font-semibold">
        {now}
      </span>
      <span>
        {state.lastSyncedAt === null
          ? "not synced yet"
          : `last synced ${time.format(new Date(state.lastSyncedAt))}`}
      </span>
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
