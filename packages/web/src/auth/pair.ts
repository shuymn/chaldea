// The pair flow (docs/design/w1-pwa.md §2.2; mvp §2.5 "Secrets"): read `#pair=<code>`,
// remove the fragment with `history.replaceState` before the exchange, then
// `POST /api/pair`. A 200 stores a new generation; a refusal, a network error, or a 503
// keeps whatever is stored. The caller asks for a fresh link when nothing is stored.

import { type Fetch, postPair } from "../http";
import type { Credential, CredentialStore } from "./credential";

export interface PairDeps {
  readonly location: Pick<Location, "hash" | "pathname" | "search">;
  readonly history: Pick<History, "replaceState">;
  readonly fetch: Fetch;
  readonly store: CredentialStore;
  /** The page's lifetime: once aborted, a late exchange is not stored. */
  readonly signal?: AbortSignal;
}

/** The code of a `#pair=<code>` fragment, or null. */
export function pairCode(hash: string): string | null {
  const m = /^#pair=([^&]+)$/.exec(hash);
  if (!m?.[1]) return null;
  try {
    return decodeURIComponent(m[1]);
  } catch {
    return null;
  }
}

/** Runs the pair flow when the page was opened from a link; resolves with the credential to use. */
export async function pairFromLink(deps: PairDeps): Promise<Credential | null> {
  const code = pairCode(deps.location.hash);
  if (code !== null) {
    // The fragment goes before the code leaves the page.
    deps.history.replaceState(null, "", deps.location.pathname + deps.location.search);
    const signal = deps.signal ?? new AbortController().signal;
    const r = await postPair(deps.fetch, code, signal);
    if (signal.aborted) return null;
    if (r.kind === "ok") return deps.store.store(r.token);
  } else if (deps.location.hash.startsWith("#pair")) {
    deps.history.replaceState(null, "", deps.location.pathname + deps.location.search);
  }
  return deps.store.read();
}
