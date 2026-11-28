// The page's command operations, bound to the runners (App.tsx builds them). Every one
// reads its gate from the sync state at the gesture; the reducer reserves the slot and
// refuses a busy, locked, or closed one with a notice.

import type { SpawnArgs } from "@chaldea/contracts/generated/SpawnArgs";

import type { CommandName } from "../commands/client";
import {
  canCancel,
  canResolve,
  canSend,
  canSpawn,
  readyLaunch,
  resumeLaunch,
  slots,
  stopLaunch,
  targetGate,
} from "../commands/controls";
import type { CommandRunner } from "../commands/runner";
import type { State } from "../sync/state";

export interface Ops {
  edit(thread: string, text: string): void;
  send(thread: string): void;
  ready(thread: string, launch: string): void;
  stop(thread: string, launch: string): void;
  resume(thread: string, launch: string): void;
  resolve(thread: string, input: string, delivered: boolean): void;
  cancel(thread: string, input: string): void;
  spawn(args: SpawnArgs): void;
  /** Retry of the attempt the control was shown for. */
  retry(slot: string, attempt: number, name: CommandName): void;
  forget(slot: string): void;
}

export function bindOps(commands: CommandRunner): Ops {
  const gesture = (
    slot: string,
    name: CommandName,
    thread: string | null,
    args: Record<string, unknown>,
    allowed: (s: State) => boolean,
  ) => {
    commands.gesture({ slot, name, thread, args, allowed });
  };
  return {
    edit: (thread, text) => commands.dispatch({ type: "edit", thread, text }),
    send: (thread) => gesture(slots.send(thread), "send", thread, {}, (s) => canSend(s, thread)),
    ready: (thread, launch) =>
      gesture(
        slots.ready(thread),
        "ready",
        thread,
        { thread_id: thread, expected_launch_id: launch },
        (s) => readyLaunch(s, thread) === launch,
      ),
    stop: (thread, launch) =>
      gesture(
        slots.launch(thread),
        "stop",
        thread,
        { thread_id: thread, expected_launch_id: launch },
        (s) => stopLaunch(s, thread) === launch,
      ),
    resume: (thread, launch) =>
      gesture(
        slots.launch(thread),
        "resume",
        thread,
        { thread_id: thread, expected_launch_id: launch },
        (s) => resumeLaunch(s, thread) === launch,
      ),
    resolve: (thread, input, delivered) =>
      gesture(slots.input(input), "input_resolve", thread, { input_id: input, delivered }, (s) =>
        canResolve(s, thread, input),
      ),
    cancel: (thread, input) =>
      gesture(slots.input(input), "input_cancel", thread, { input_id: input }, (s) => canCancel(s, thread, input)),
    spawn: (args) => gesture(slots.spawn, "spawn", null, { ...args }, canSpawn),
    retry: (slot, attempt, name) => {
      const c = commands.getState().slots[slot];
      // The Retry gate the control shows (controls.ts `canRetry`); the reducer refuses the
      // Retry, with a notice, when a newer command now holds the slot.
      if (c) commands.retry(slot, attempt, name, targetGate(c));
    },
    forget: (slot) => commands.dispatch({ type: "forget", slot }),
  };
}
