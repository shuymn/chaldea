// Entry: start the sync client (which pairs from the link if there is one) and render.
// State lives in memory only; there is no service worker (docs/design/w1-pwa.md D7).

import "./index.css";

import { createRoot } from "react-dom/client";

import { CredentialStore } from "./auth/credential";
import { pairFromLink } from "./auth/pair";
import { browserCommands } from "./commands/runner";
import { browserRunner } from "./sync/runner";
import { App } from "./ui/App";

const root = document.getElementById("root");
if (root) {
  const store = new CredentialStore();
  const runner = browserRunner(store);
  const commands = browserCommands(runner);
  const view = createRoot(root);
  view.render(<App runner={runner} commands={commands} />);
  void runner.start((signal) =>
    pairFromLink({ location, history, fetch: (i, init) => fetch(i, init), store, signal }),
  );
  // Best effort: ask the browser not to evict the credential under storage pressure.
  void navigator.storage?.persist?.().catch(() => false);
  // Leaving the page ends every request (commands included), socket, and timer. A page
  // restored from the back-forward cache reloads, which reads everything from the daemon again.
  addEventListener("pagehide", () => {
    commands.stop();
    runner.stop();
    store.close();
    view.unmount();
  });
  addEventListener("pageshow", (e) => {
    if (e.persisted) location.reload();
  });
}
