// Entry: pair from the link if there is one, then start the sync client and render.
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
  createRoot(root).render(<App runner={runner} />);
  pairFromLink({ location, history, fetch: (i, init) => fetch(i, init), store })
    .catch(() => null)
    .then((cred) => {
      runner.start(cred);
      // Best effort: ask the browser not to evict the credential under storage pressure.
      void navigator.storage?.persist?.().catch(() => false);
    });
}
