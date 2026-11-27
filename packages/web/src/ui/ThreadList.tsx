// The thread list: agent, cwd, the three axes, attention, and closed. Axes of a row
// without a valid thread copy are the host's, labelled "last known" or "unavailable"
// (F1); a followed row whose stream failed shows the failure and a retry. The rows on
// screen (IntersectionObserver) are reported once scrolling settles: no scroll event and
// no visibility change for 250 ms.

import { useEffect, useRef } from "react";

import { listRows, type RowView, rowView, type State } from "../sync/state";

export const SETTLE_MS = 250;

const label = (s: string) => s.replaceAll("_", " ");

function Row(props: {
  view: RowView;
  selected: boolean;
  onSelect: (thread: string) => void;
  onRetry: () => void;
}) {
  const { view, selected } = props;
  const { info } = view;
  const attention = view.attention.map((a) => label(a.kind)).join(", ");
  return (
    <li data-thread={info.thread_id} className="border-b border-neutral-300 dark:border-neutral-700">
      <button
        type="button"
        aria-current={selected ? "true" : undefined}
        onClick={() => props.onSelect(info.thread_id)}
        className="block w-full px-3 py-2 text-start hover:bg-neutral-100 aria-[current=true]:bg-neutral-200 dark:hover:bg-neutral-800 dark:aria-[current=true]:bg-neutral-700"
      >
        <span className="flex flex-wrap items-baseline gap-x-2">
          <span className="font-semibold">{info.agent}</span>
          {info.closed && <span className="text-sm">closed</span>}
          {view.label && <span className="text-sm italic">{view.label}</span>}
        </span>
        <span className="block break-all text-sm" dir="auto">
          {info.cwd}
        </span>
        <span className="block text-sm">
          {label(view.process)} · {label(view.turn)} · task {label(view.task)}
        </span>
        {attention && <span className="block text-sm font-semibold">attention: {attention}</span>}
      </button>
      {view.error && (
        <p className="flex flex-wrap items-center gap-2 px-3 pb-2 text-sm">
          <span>{view.error.message}</span>
          <button type="button" className="control" onClick={props.onRetry}>
            Retry
          </button>
        </p>
      )}
    </li>
  );
}

export function ThreadList(props: {
  state: State;
  onSelect: (thread: string) => void;
  onVisible: (threads: string[]) => void;
  onRetry: () => void;
}) {
  const { state, onVisible } = props;
  const rows = listRows(state);
  const listRef = useRef<HTMLUListElement>(null);
  const visible = useRef(new Set<string>());
  const settle = useRef<ReturnType<typeof setTimeout> | null>(null);
  const report = useRef(onVisible);
  report.current = onVisible;
  const ids = rows.map((r) => r.info.thread_id).join(" ");

  useEffect(() => {
    const list = listRef.current;
    if (!list || typeof IntersectionObserver === "undefined") return;
    // Every visibility change and every scroll restarts the settle timer.
    const restart = () => {
      if (settle.current !== null) clearTimeout(settle.current);
      settle.current = setTimeout(() => {
        settle.current = null;
        report.current([...visible.current]);
      }, SETTLE_MS);
    };
    const io = new IntersectionObserver((entries) => {
      for (const e of entries) {
        const t = (e.target as HTMLElement).dataset.thread;
        if (!t) continue;
        if (e.isIntersecting) visible.current.add(t);
        else visible.current.delete(t);
      }
      restart();
    });
    visible.current = new Set();
    for (const li of list.querySelectorAll<HTMLElement>("li[data-thread]")) io.observe(li);
    // Capture: a scroll of the page or of any container scrolls the rows.
    addEventListener("scroll", restart, { capture: true, passive: true });
    return () => {
      io.disconnect();
      removeEventListener("scroll", restart, { capture: true });
      if (settle.current !== null) clearTimeout(settle.current);
    };
  }, [ids]);

  return (
    <nav aria-labelledby="threads-heading" className="min-w-0">
      <h2 id="threads-heading" className="px-3 py-2 text-lg font-semibold">
        Threads
      </h2>
      {rows.length === 0 ? (
        <p className="px-3 py-2">No threads yet.</p>
      ) : (
        <ul ref={listRef}>
          {rows.map((r) => (
            <Row
              key={r.info.thread_id}
              view={rowView(state, r)}
              selected={state.selected === r.info.thread_id}
              onSelect={props.onSelect}
              onRetry={props.onRetry}
            />
          ))}
        </ul>
      )}
    </nav>
  );
}
