// The page: header with the status, the thread list, and the timeline. Read-only in W1:
// no composer, commands, or terminal.

import { useCallback, useSyncExternalStore } from "react";

import type { Runner } from "../sync/runner";
import { StatusBar } from "./StatusBar";
import { ThreadList } from "./ThreadList";
import { Timeline } from "./Timeline";

export function App(props: { runner: Runner }) {
  const { runner } = props;
  const state = useSyncExternalStore(runner.subscribe, runner.getState);
  const select = useCallback((thread: string) => runner.dispatch({ type: "select", thread }), [runner]);
  const retry = useCallback(() => runner.dispatch({ type: "retry" }), [runner]);
  const visible = useCallback(
    (threads: string[]) => runner.dispatch({ type: "visible", threads }),
    [runner],
  );
  const host = state.streams.host;
  return (
    <div className="page">
      <a href="#main" className="skip-link">
        Skip to the timeline
      </a>
      <header className="flex flex-wrap items-center gap-x-4 gap-y-1 border-b border-neutral-300 px-3 py-2 dark:border-neutral-700">
        <h1 className="text-xl font-bold">Chaldea</h1>
        <StatusBar state={state} />
      </header>
      {state.storageFailed && state.generation === undefined ? (
        <main id="main" className="p-3">
          <p className="flex flex-wrap items-center gap-2">
            <span>
              Browser storage is unavailable, so the pairing cannot be read. Retry, or run{" "}
              <code>chaldea open</code> again.
            </span>
            <button type="button" className="control" onClick={() => void runner.retryStorage()}>
              Retry
            </button>
          </p>
        </main>
      ) : state.generation === null ? (
        <main id="main" className="p-3">
          <p>
            This link is spent or expired; run <code>chaldea open</code> again.
          </p>
        </main>
      ) : (
        <div className="layout">
          {host?.error && (
            <p className="px-3 py-2">
              {host.error.message}{" "}
              <button type="button" className="control" onClick={retry}>
                Retry
              </button>
            </p>
          )}
          <ThreadList state={state} onSelect={select} onVisible={visible} onRetry={retry} />
          <main id="main" tabIndex={-1} className="min-w-0">
            <Timeline
              state={state}
              onOlder={() => runner.dispatch({ type: "older" })}
              onNewer={() => runner.dispatch({ type: "newer" })}
              onLatest={() => runner.dispatch({ type: "latest" })}
              onRetry={retry}
            />
          </main>
        </div>
      )}
    </div>
  );
}
