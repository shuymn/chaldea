// The commands' runner: holds the browser handles of the command reducer (fetches, the
// Retry timer) beside the sync runner, whose state it reads for the gates and the session.
// Every POST it makes answers exactly one `reply`; each has the command's deadline
// (client.ts), and `stop` (the page unloading) aborts them and the timer.

import type { Fetch } from "../http";
import type { Runner } from "../sync/runner";
import type { State } from "../sync/state";
import { type CommandName, postCommand } from "./client";
import { context, refusal, sameContext, threadLabel } from "./controls";
import { type CmdAction, type CmdEffect, type CmdState, initialCmdState, reduceCommands } from "./reducer";

type DistributiveOmit<T, K extends PropertyKey> = T extends unknown ? Omit<T, K> : never;
export type CmdInput = DistributiveOmit<CmdAction, "now">;

export interface CommandDeps {
  readonly fetch: Fetch;
  readonly now: () => number;
  readonly uuid: () => string;
}

export class CommandRunner {
  private state: CmdState;
  private readonly listeners = new Set<() => void>();
  private readonly aborts = new Map<number, AbortController>();
  private timer: ReturnType<typeof setTimeout> | null = null;
  private readonly unsubscribe: () => void;
  private stopped = false;

  constructor(
    private readonly sync: Runner,
    private readonly deps: CommandDeps,
  ) {
    this.state = initialCmdState(deps.now());
    this.unsubscribe = sync.subscribe(() => this.follow());
    this.follow();
  }

  readonly subscribe = (l: () => void): (() => void) => {
    this.listeners.add(l);
    return () => this.listeners.delete(l);
  };

  readonly getState = (): CmdState => this.state;

  /** The sync side changed: pass its session context on when it differs. */
  private follow(): void {
    const ctx = context(this.sync.getState());
    if (!sameContext(ctx, this.state.ctx)) this.dispatch({ type: "context", ctx });
  }

  /**
   * A gesture: `allowed` is the gate read from the sync state now, and the command id is
   * minted here, once, then frozen with the body.
   */
  gesture(opts: {
    slot: string;
    name: CommandName;
    thread: string | null;
    args: Record<string, unknown>;
    allowed: (s: State) => boolean;
  }): void {
    const s = this.sync.getState();
    const generation = typeof s.generation === "string" ? s.generation : null;
    if (generation === null) return;
    this.follow();
    this.dispatch({
      type: "gesture",
      slot: opts.slot,
      name: opts.name,
      thread: opts.thread,
      label: opts.thread === null ? null : threadLabel(s, opts.thread),
      args: opts.args,
      allowed: opts.allowed(s),
      why: refusal(s, opts.name, opts.thread),
      commandId: this.deps.uuid(),
      generation,
    });
  }

  retry(slot: string, attempt: number, name: CommandName, allowed: (s: State) => boolean): void {
    this.follow();
    const s = this.sync.getState();
    const c = this.state.slots[slot];
    this.dispatch({ type: "retry", slot, attempt, name, allowed: allowed(s), why: refusal(s, name, c?.thread ?? null) });
  }

  dispatch(input: CmdInput): void {
    if (this.stopped) return;
    const [next, effects] = reduceCommands(this.state, { ...input, now: this.deps.now() } as CmdAction);
    this.state = next;
    for (const e of effects) this.run(e);
    for (const l of this.listeners) l();
  }

  private run(e: CmdEffect): void {
    switch (e.type) {
      case "post": {
        const token = this.sync.token(e.generation);
        const attempt = e.attempt;
        if (token === null) {
          queueMicrotask(() => this.dispatch({ type: "reply", attempt, reply: { kind: "not_sent" } }));
          return;
        }
        const ac = new AbortController();
        this.aborts.set(attempt, ac);
        void postCommand(this.deps.fetch, e.name, e.body, token, ac.signal).then((reply) => {
          this.aborts.delete(attempt);
          this.dispatch({ type: "reply", attempt, reply });
        });
        return;
      }
      case "unauthorized":
        this.sync.dispatch({ type: "commandUnauthorized", generation: e.generation });
        return;
      case "select":
        this.sync.dispatch({ type: "select", thread: e.thread });
        return;
      case "wake": {
        if (this.timer !== null) clearTimeout(this.timer);
        this.timer = null;
        if (e.at === null) return;
        this.timer = setTimeout(
          () => {
            this.timer = null;
            this.dispatch({ type: "tick" });
          },
          Math.max(0, e.at - this.deps.now()),
        );
        return;
      }
    }
  }

  /** Ends every request and the timer; nothing is dispatched after. */
  stop(): void {
    this.stopped = true;
    this.unsubscribe();
    for (const ac of this.aborts.values()) ac.abort();
    this.aborts.clear();
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = null;
  }
}

export function browserCommands(sync: Runner): CommandRunner {
  return new CommandRunner(sync, {
    fetch: (input, init) => fetch(input, init),
    now: () => Date.now(),
    uuid: () => crypto.randomUUID(),
  });
}
