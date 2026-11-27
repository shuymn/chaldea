// Entry: start the sync client (which pairs from the link if there is one) and render.
// State lives in memory only; there is no service worker (docs/design/w1-pwa.md D7).

import "./index.css";

import { createRoot } from "react-dom/client";

import { CredentialStore } from "./auth/credential";
import { pairFromLink } from "./auth/pair";
import { browserRunner } from "./sync/runner";
import { App } from "./ui/App";

const root = document.getElementById("root");
if (root) {
  const store = new CredentialStore();
  const runner = browserRunner(store);
  const view = createRoot(root);
  view.render(<App runner={runner} />);
  void runner.start(() => pairFromLink({ location, history, fetch: (i, init) => fetch(i, init), store }));
  // Best effort: ask the browser not to evict the credential under storage pressure.
  void navigator.storage?.persist?.().catch(() => false);
  // Leaving the page ends every request, socket, and timer. A page restored from the
  // back-forward cache starts again rather than resume a stopped client.
  addEventListener("pagehide", () => {
    runner.stop();
    view.unmount();
  });
  addEventListener("pageshow", (e) => {
    if (e.persisted) location.reload();
  });
}
