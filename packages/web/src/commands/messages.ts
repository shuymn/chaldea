// What the page says about a command's reply: the codes W2's commands can return, and a
// generic text for the rest. No URL, body, or header appears in any of them.

import type { ErrorCode } from "@chaldea/contracts/generated/ErrorCode";
import type { RejectReason } from "@chaldea/contracts/generated/RejectReason";

import type { Reply } from "./client";
import type { CmdState, Command, Notice } from "./reducer";

const CODES: Partial<Record<ErrorCode, string>> = {
  bad_args: "the daemon refused the arguments",
  forbidden: "this pairing may not do that",
  not_found: "the thread or input no longer exists",
  payload_too_large: "the request is too large",
  conflict: "this command id was already used for another request",
  unknown_after_crash: "the daemon restarted while it ran; its outcome is unknown",
  not_implemented: "the daemon does not support this command",
  busy: "the daemon is busy",
  shutting_down: "the daemon is shutting down",
  disk_full: "the daemon's disk is full",
  store_failed: "the daemon could not write its store",
  request_timeout: "the request ran out of time before it was recorded",
  thread_closed: "the thread is closed",
  launch_changed: "the thread's launch changed; review it and try again",
  launch_live: "the launch is still running",
  death_unconfirmed: "the previous launch is not confirmed dead",
  worktree_unavailable: "the thread's worktree is unavailable",
  no_session: "the thread has no session to resume",
  prompt_rejected: "the prompt was refused",
  prompt_too_large: "the prompt is over 256 KiB",
  launch_busy: "the launch was busy; nothing changed",
  no_live_launch: "the thread has no running launch",
  not_bound: "the launch has no bound session yet",
  input_state: "the input is no longer in that state",
  unit_running: "another write is still running",
};

const REJECT: Record<RejectReason, string> = {
  control_character: "it holds a control character",
  lossy_text: "the CLI would not receive it as written",
  empty_prompt: "it is empty",
  leading_trailing_whitespace: "it starts or ends with whitespace",
  tab: "it holds a tab",
  spawn_failed: "the launch failed to start",
};

export const rejectText = (r: RejectReason) => REJECT[r];

/** The hint for an exited launch whose death is not confirmed (mvp:216). */
export function hintText(thread: string, launch: string): string {
  return `Resume waits until the previous launch is confirmed dead. Review with "chaldea stop ${thread} --assume-dead", then confirm with "--launch ${launch}".`;
}

export const INVALID_TEXT = {
  empty: "Write a message first.",
  too_large: "The message is over 256 KiB of UTF-8; shorten it to send.",
} as const;

/** The status bar's line for the latest notice; `label` names the thread. */
export function noticeText(cmds: CmdState, n: Notice, label: (thread: string) => string): string | null {
  const where = n.thread === null ? "" : `${label(n.thread)}: `;
  if (n.kind === "invalid") {
    const why = n.thread === null ? undefined : cmds.invalid[n.thread];
    return why === undefined ? null : `${where}${INVALID_TEXT[why]}`;
  }
  const c = cmds.slots[n.slot];
  return c === undefined ? null : `${where}${commandText(c)}`;
}

const codeText =(code: ErrorCode | null) => (code !== null ? (CODES[code] ?? `refused (${code})`) : null);

/** The reply's text, for the command's own result line. */
export function replyText(r: Reply): string {
  switch (r.kind) {
    case "ok":
      return "Done.";
    case "unreadable":
      return "Done, but the reply could not be read.";
    case "settled":
      return `Refused: ${codeText(r.code) ?? `the daemon answered ${r.status}`}.`;
    case "unknown":
      return "The outcome is unknown: check the thread before sending anything again.";
    case "not_done":
      return `Not done: ${codeText(r.code) ?? "the request was not recorded"}.`;
    case "unauthorized":
      return "Not done: the pairing was refused.";
    case "not_sent":
      return "Not sent.";
  }
}

const VERB: Record<Command["name"], string> = {
  send: "Send",
  ready: "Mark ready",
  input_resolve: "Resolve",
  input_cancel: "Cancel",
  spawn: "New thread",
  stop: "Stop",
  resume: "Resume",
};

/** One line for a command: its state, then its result. */
export function commandText(c: Command): string {
  const verb = VERB[c.name];
  switch (c.phase) {
    case "inflight":
      return `${verb}: sending…`;
    case "check_thread":
      return `${verb}: the outcome is unknown. Check the thread; nothing is resent.`;
    case "forgotten":
      return `${verb}: forgotten. Sending again is a new command.`;
    case "not_done":
    case "released":
      return `${verb}: ${c.reply ? replyText(c.reply) : "not done."}`;
    case "settled":
      return `${verb}: ${c.reply ? resultText(c) : "done."}`;
  }
}

function resultText(c: Command): string {
  const r = c.reply;
  if (r?.kind !== "ok") return r ? replyText(r) : "done.";
  const v = r.result;
  if ("reject_reason" in v) {
    return v.state === "rejected" && v.reject_reason !== null
      ? `rejected: ${rejectText(v.reject_reason)}.`
      : `${v.state.replaceAll("_", " ")}.`;
  }
  if ("death_confirmed" in v) {
    return v.death_confirmed ? `${v.outcome.replaceAll("_", " ")}, death confirmed.` : `${v.outcome.replaceAll("_", " ")}, death not confirmed.`;
  }
  if ("outcome" in v) return `outcome: ${v.outcome}.`;
  if ("state" in v) return `${v.state}.`;
  return "done.";
}
