// The page's command operations, bound to the runners (App.tsx builds them). Every one
// reads its gate from the sync state at the gesture; the reducer reserves the slot.

import type { SpawnArgs } from "@chaldea/contracts/generated/SpawnArgs";

import {
  canCancel,
  canResolve,
  canSend,
  canSpawn,
  lockReason,
  readyLaunch,
  resumeLaunch,
  slots,
  stopLaunch,
} from "../commands/controls";
import type { Command } from "../commands/reducer";
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
  retry(slot: string): void;
  forget(slot: string): void;
}

/** The gate a command's Retry needs: the same as its first dispatch. */
function gateOf(c: Command): (s: State) => boolean {
  const t = c.thread ?? "";
  switch (c.name) {
    case "send":
      return (s) => canSend(s, t);
    case "ready":
      return (s) => readyLaunch(s, t) !== null;
    case "stop":
      return (s) => stopLaunch(s, t) !== null;
    case "resume":
      return (s) => resumeLaunch(s, t) !== null;
    case "input_resolve":
    case "input_cancel":
      return (s) => lockReason(s, t) === null;
    case "spawn":
      return canSpawn;
  }
}

export function bindOps(commands: CommandRunner): Ops {
  return {
    edit: (thread, text) => commands.dispatch({ type: "edit", thread, text }),
    send: (thread) =>
      commands.gesture({ slot: slots.send(thread), name: "send", thread, args: {}, allowed: (s) => canSend(s, thread) }),
    ready: (thread, launch) =>
      commands.gesture({
        slot: slots.ready(thread),
        name: "ready",
        thread,
        args: { thread_id: thread, expected_launch_id: launch },
        allowed: (s) => readyLaunch(s, thread) === launch,
      }),
    stop: (thread, launch) =>
      commands.gesture({
        slot: slots.launch(thread),
        name: "stop",
        thread,
        args: { thread_id: thread, expected_launch_id: launch },
        allowed: (s) => stopLaunch(s, thread) === launch,
      }),
    resume: (thread, launch) =>
      commands.gesture({
        slot: slots.launch(thread),
        name: "resume",
        thread,
        args: { thread_id: thread, expected_launch_id: launch },
        allowed: (s) => resumeLaunch(s, thread) === launch,
      }),
    resolve: (thread, input, delivered) =>
      commands.gesture({
        slot: slots.input(input),
        name: "input_resolve",
        thread,
        args: { input_id: input, delivered },
        allowed: (s) => canResolve(s, thread, input),
      }),
    cancel: (thread, input) =>
      commands.gesture({
        slot: slots.input(input),
        name: "input_cancel",
        thread,
        args: { input_id: input },
        allowed: (s) => canCancel(s, thread, input),
      }),
    spawn: (args) =>
      commands.gesture({ slot: slots.spawn, name: "spawn", thread: null, args: { ...args }, allowed: canSpawn }),
    retry: (slot) => {
      const c = commands.getState().slots[slot];
      if (c) commands.retry(slot, gateOf(c));
    },
    forget: (slot) => commands.dispatch({ type: "forget", slot }),
  };
}
