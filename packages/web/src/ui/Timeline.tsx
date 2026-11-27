// The displayed thread's timeline: the history window's events in `seq` order as plain
// text (`pre-wrap`, `dir="auto"`), with the window controls Older, Newer, and Jump to
// latest (docs/design/w1-pwa.md §2.4, §2.5). An `<ol>` with no live region: new events
// are announced by the page's one status region.

import { listRows, rowView, type State, threadStream } from "../sync/state";
import { eventText } from "./eventText";

const time = new Intl.DateTimeFormat(undefined, { dateStyle: "short", timeStyle: "medium" });

function when(ts: string): string {
  const d = new Date(ts);
  return Number.isNaN(d.getTime()) ? ts : time.format(d);
}

export function Timeline(props: {
  state: State;
  onOlder: () => void;
  onNewer: () => void;
  onLatest: () => void;
  onRetry: () => void;
}) {
  const { state } = props;
  const selected = state.selected;
  if (selected === null) {
    return (
      <section aria-labelledby="timeline-heading" className="min-w-0 p-3">
        <h2 id="timeline-heading" className="text-lg font-semibold">
          Timeline
        </h2>
        <p>Select a thread to read its timeline.</p>
      </section>
    );
  }
  const row = listRows(state).find((r) => r.info.thread_id === selected);
  const view = row ? rowView(state, row) : null;
  const stream = state.streams[threadStream(selected)];
  const cursor = stream?.copy?.cursor ?? null;
  const w = state.window?.thread === selected ? state.window : null;
  const canOlder = w !== null && w.pending === null && w.lo > 1;
  const canNewer = w !== null && w.pending === null && cursor !== null && w.hi < cursor;
  const error = w?.error ?? stream?.error ?? null;
  return (
    <section aria-labelledby="timeline-heading" className="min-w-0">
      <div className="flex flex-wrap items-center gap-2 p-3">
        <h2 id="timeline-heading" className="me-auto min-w-0 text-lg font-semibold">
          <span>{view?.info.agent ?? "thread"}</span>{" "}
          <span className="break-all text-sm font-normal" dir="auto">
            {view?.info.cwd ?? selected}
          </span>
        </h2>
        <div className="flex flex-wrap gap-2" role="group" aria-label="History">
          <button type="button" className="control" disabled={!canOlder} onClick={props.onOlder}>
            Older
          </button>
          <button type="button" className="control" disabled={!canNewer} onClick={props.onNewer}>
            Newer
          </button>
          <button
            type="button"
            className="control"
            disabled={w === null || (w.attached && w.pending === null)}
            onClick={props.onLatest}
          >
            Jump to latest
          </button>
        </div>
      </div>
      {error && (
        <p className="flex flex-wrap items-center gap-2 px-3 pb-2">
          <span>{error.message}</span>
          <button type="button" className="control" onClick={props.onRetry}>
            Retry
          </button>
        </p>
      )}
      {w === null ? (
        <p className="px-3">Loading the timeline…</p>
      ) : w.events.length === 0 && w.pending === null ? (
        <p className="px-3">No events yet.</p>
      ) : (
        <ol className="px-3 pb-3">
          {w.events.map((e) => {
            const t = eventText(e);
            return (
              <li key={e.seq} className="border-b border-neutral-300 py-2 dark:border-neutral-700">
                <p className="flex flex-wrap gap-x-2 text-sm">
                  <span>#{e.seq}</span>
                  <time dateTime={e.occurred_at}>{when(e.occurred_at)}</time>
                  <span className="font-semibold">{t.title}</span>
                  {t.preview && <span className="italic">preview</span>}
                </p>
                {t.body !== null && (
                  <p className="whitespace-pre-wrap break-words" dir="auto">
                    {t.body}
                  </p>
                )}
              </li>
            );
          })}
        </ol>
      )}
      {w !== null && !w.attached && (
        <p className="px-3 pb-3 text-sm">Showing older history; live events are not added here.</p>
      )}
    </section>
  );
}
