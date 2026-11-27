// The paired credential (docs/design/w1-pwa.md §2.2, D3): one IndexedDB record
// `{ generation, token }`. A 401 deletes it only when the stored generation is the
// request's, in one compare-and-delete transaction; every committed change is posted on
// a BroadcastChannel after the transaction completes, and receivers reread the store
// rather than trust the message.

export interface Credential {
  /** A random UUID per stored token; every request carries it. */
  readonly generation: string;
  readonly token: string;
}

/** What a tab posts after a committed change (no token, only the generation). */
export type CredentialNotice = { readonly stored: string } | { readonly cleared: string };

const DB_NAME = "chaldea";
const STORE = "credential";
const KEY = "current";
export const CHANNEL = "chaldea-auth";

export interface CredentialDeps {
  readonly indexedDB: IDBFactory;
  readonly channel: () => BroadcastChannel;
  readonly uuid: () => string;
}

const browserDeps = (): CredentialDeps => ({
  indexedDB: globalThis.indexedDB,
  channel: () => new BroadcastChannel(CHANNEL),
  uuid: () => crypto.randomUUID(),
});

function isCredential(v: unknown): v is Credential {
  if (typeof v !== "object" || v === null) return false;
  const r = v as Record<string, unknown>;
  return typeof r.generation === "string" && typeof r.token === "string";
}

export class CredentialStore {
  private db: Promise<IDBDatabase> | null = null;
  private readonly deps: CredentialDeps;

  constructor(deps: CredentialDeps = browserDeps()) {
    this.deps = deps;
  }

  private open(): Promise<IDBDatabase> {
    this.db ??= new Promise<IDBDatabase>((resolve, reject) => {
      const req = this.deps.indexedDB.open(DB_NAME, 1);
      req.onupgradeneeded = () => {
        req.result.createObjectStore(STORE);
      };
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(new Error("the credential store could not be opened"));
    }).catch((e: unknown) => {
      // A failed open is not kept: the next call (a Retry) opens again.
      this.db = null;
      throw e;
    });
    return this.db;
  }

  /** Runs one transaction; resolves with `body`'s value once it commits. */
  private async tx<T>(
    mode: IDBTransactionMode,
    body: (store: IDBObjectStore, done: (v: T) => void) => void,
  ): Promise<T> {
    const db = await this.open();
    return new Promise<T>((resolve, reject) => {
      const t = db.transaction(STORE, mode);
      let value: T | undefined;
      let set = false;
      body(t.objectStore(STORE), (v) => {
        value = v;
        set = true;
      });
      t.oncomplete = () => {
        if (set) resolve(value as T);
        else reject(new Error("the credential transaction set no result"));
      };
      t.onabort = () => reject(new Error("the credential transaction was aborted"));
      t.onerror = () => reject(new Error("the credential transaction failed"));
    });
  }

  async read(): Promise<Credential | null> {
    return this.tx<Credential | null>("readonly", (store, done) => {
      const get = store.get(KEY);
      get.onsuccess = () => done(isCredential(get.result) ? get.result : null);
    });
  }

  /** Stores a new generation for `token` and posts it after the commit. */
  async store(token: string): Promise<Credential> {
    const cred: Credential = { generation: this.deps.uuid(), token };
    await this.tx<true>("readwrite", (store, done) => {
      const put = store.put(cred, KEY);
      put.onsuccess = () => done(true);
    });
    this.post({ stored: cred.generation });
    return cred;
  }

  /**
   * The 401 path: deletes the record only if its generation is `generation`, in one
   * transaction, and posts the clear after the commit; an empty record posts no second
   * clear. Resolves with what is stored afterwards; the runner does not use it, it
   * rereads the store under the 401's generation instead.
   */
  async clearIf(generation: string): Promise<Credential | null> {
    const result = await this.tx<{ deleted: boolean; current: Credential | null }>(
      "readwrite",
      (store, done) => {
        const get = store.get(KEY);
        get.onsuccess = () => {
          const cur = isCredential(get.result) ? get.result : null;
          if (cur?.generation === generation) {
            const del = store.delete(KEY);
            del.onsuccess = () => done({ deleted: true, current: null });
          } else {
            done({ deleted: false, current: cur });
          }
        };
      },
    );
    if (result.deleted) this.post({ cleared: generation });
    return result.current;
  }

  /**
   * Closes the connection once any open in progress finishes; transactions already
   * created complete first. For the page's teardown, after the runner stopped.
   */
  close(): void {
    const db = this.db;
    this.db = null;
    void db?.then(
      (d) => d.close(),
      () => {},
    );
  }

  /** Calls `onChange` when another tab posts a committed change; returns the unsubscribe. */
  listen(onChange: () => void): () => void {
    const ch = this.deps.channel();
    ch.onmessage = () => onChange();
    return () => ch.close();
  }

  private post(notice: CredentialNotice): void {
    const ch = this.deps.channel();
    ch.postMessage(notice);
    ch.close();
  }
}
