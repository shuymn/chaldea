// The page: header with the status, the attention banner, the thread list with the
// new-thread form, and the selected thread's actions, inputs, composer, and timeline
// (docs/design/w1-pwa.md, docs/design/w2-composer.md). No terminal view.

import { useCallback, useMemo, useSyncExternalStore } from "react";

import type { CommandRunner } from "../commands/runner";
import type { Runner } from "../sync/runner";
import { AttentionBanner } from "./AttentionBanner";
import { Composer } from "./Composer";
import { Inputs } from "./Inputs";
import { bindOps } from "./ops";
import { SpawnForm } from "./SpawnForm";
import { StatusBar } from "./StatusBar";
import { ThreadActions } from "./ThreadActions";
import { ThreadList } from "./ThreadList";
import { Timeline } from "./Timeline";

export function App(props: { runner: Runner; commands: CommandRunner }) {
  const { runner, commands } = props;
  const state = useSyncExternalStore(runner.subscribe, runner.getState);
  const cmds = useSyncExternalStore(commands.subscribe, commands.getState);
  const ops = useMemo(() => bindOps(commands), [commands]);
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
        {host?.copy?.kind === "host" && (
          <span className="text-sm" data-raw-unknown="host">
            host raw unknown events: {host.copy.rawUnknown}
          </span>
        )}
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
        <>
          <AttentionBanner state={state} cmds={cmds} ops={ops} />
          <div className="layout">
            {host?.error && (
              <p className="px-3 py-2">
                {host.error.message}{" "}
                <button type="button" className="control" onClick={retry}>
                  Retry
                </button>
              </p>
            )}
            <div className="min-w-0">
              <ThreadList state={state} onSelect={select} onVisible={visible} onRetry={retry} />
              <SpawnForm state={state} cmds={cmds} ops={ops} />
            </div>
            <main id="main" tabIndex={-1} className="min-w-0">
              {state.selected !== null && (
                // Keyed by thread: a switch resets the composer's IME flag and timestamp.
                <div key={state.selected} className="pt-2">
                  <ThreadActions state={state} cmds={cmds} thread={state.selected} ops={ops} />
                  <Inputs state={state} cmds={cmds} thread={state.selected} ops={ops} />
                  <Composer state={state} cmds={cmds} thread={state.selected} ops={ops} />
                </div>
              )}
              <Timeline
                state={state}
                onOlder={() => runner.dispatch({ type: "older" })}
                onNewer={() => runner.dispatch({ type: "newer" })}
                onLatest={() => runner.dispatch({ type: "latest" })}
                onRetry={retry}
              />
            </main>
          </div>
        </>
      )}
    </div>
  );
}
