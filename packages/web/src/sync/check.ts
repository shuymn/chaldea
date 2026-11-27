// What "converged" means after a quiesce (W1-5): the server's epoch, cursors, owned
// projections, and the exact history window with the range its loads and navigation
// should retain (test only). An unresolved history read is not converged; a failed one
// that shows its Retry is reported as such (`HISTORY_FAILED`), never as an exact window.

import type { Sim } from "./sim";
import { HOST, listRows, rowView, status, threadOf } from "./state";

const bytesOf = (e: unknown) => new TextEncoder().encode(JSON.stringify(e)).length;

/** The prefix of the problem a failed history read with a visible Retry reports. */
export const HISTORY_FAILED = "history read failed, Retry shown";

export function problems(sim: Sim): string[] {
  const out: string[] = [];
  const s = sim.state;
  const srv = sim.server;
  if (s.epoch !== srv.db.epoch) out.push(`epoch ${s.epoch} != ${srv.db.epoch}`);
  if (s.socket?.phase !== "open" || s.socket.epoch !== srv.db.epoch) out.push("socket not live");
  if (status(s) !== "live") out.push(`status ${status(s)}`);
  if (!s.followed.includes(HOST)) out.push("host not followed");
  for (const key of s.followed) {
    const st = s.streams[key];
    if (!st) {
      out.push(`${key}: no stream state`);
      continue;
    }
    if (!srv.exists(key)) {
      out.push(`${key}: followed but missing on the server`);
      continue;
    }
    if (st.error) out.push(`${key}: error ${st.error.message}`);
    const copy = st.copy;
    if (!copy) {
      out.push(`${key}: no copy`);
      continue;
    }
    if (copy.cursor !== srv.head(key)) out.push(`${key}: cursor ${copy.cursor} != head ${srv.head(key)}`);
    if (copy.kind === "host") {
      const want = srv.threadIds().sort();
      const got = Object.keys(copy.threads).sort();
      if (want.join() !== got.join()) out.push(`host: threads ${got} != ${want}`);
      for (const id of want) {
        const row = copy.threads[id];
        const t = srv.thread(id);
        if (row && t && (row.info.closed !== t.closed || row.info.cwd !== t.cwd || row.info.agent !== t.agent)) {
          out.push(`host: row ${id} differs`);
        }
      }
    } else {
      const id = threadOf(key) ?? "";
      const t = srv.thread(id);
      const row = listRows(s).find((r) => r.info.thread_id === id);
      if (!t) continue;
      if (!row) {
        out.push(`${key}: followed without a row`);
        continue;
      }
      const v = rowView(s, row);
      const want = t.axes;
      if (v.label !== null) out.push(`${key}: labelled ${v.label}`);
      if (v.process !== want.process || v.turn !== want.turn || v.task !== want.task) {
        out.push(`${key}: axes ${v.process}/${v.turn}/${v.task} != ${want.process}/${want.turn}/${want.task}`);
      }
      if (JSON.stringify(v.attention) !== JSON.stringify(want.attention)) out.push(`${key}: attention differs`);
    }
  }
  // The followed set: every thread the server has and the client shows, within the cap.
  const sel = s.selected;
  if (sel !== null) {
    const key = `thread:${sel}`;
    if (!srv.exists(key)) out.push("selected a missing thread");
    else if (!s.followed.includes(key)) out.push("selected thread not followed");
    out.push(...windowProblems(sim, sel));
  }
  for (const r of listRows(s)) {
    if (!srv.thread(r.info.thread_id)) out.push(`row ${r.info.thread_id} not on the server`);
  }
  return out;
}

export function windowProblems(sim: Sim, thread: string): string[] {
  const out: string[] = [];
  const s = sim.state;
  const key = `thread:${thread}`;
  const w = s.window;
  const head = sim.server.head(key);
  if (!w || w.thread !== thread) return ["no window for the selected thread"];
  // A failed history read is not retried automatically: it shows its error and a Retry.
  // The window it leaves is still checked below, before any Retry.
  if (w.error) {
    out.push(`${HISTORY_FAILED}: ${w.error.message}`);
    if (!w.pending) out.push("window error without a pending read");
  } else if (w.pending) {
    out.push(`history read unresolved: ${w.pending.purpose}`);
  }
  const log = sim.server.log(key);
  const want = log.slice(w.lo - 1, w.hi);
  if (w.events.length !== w.hi - w.lo + 1) out.push("window length != hi - lo + 1");
  if (JSON.stringify(w.events) !== JSON.stringify(want)) out.push(`window ${w.lo}..${w.hi} differs from the log`);
  if (w.hi > head) out.push(`window hi ${w.hi} > head ${head}`);
  if (w.attached && w.hi !== head) out.push(`attached window ends at ${w.hi}, head ${head}`);
  // The retained range the navigation intent asks for (oracle.ts).
  out.push(...sim.oracle.problems(s));
  if (w.events.length > s.cfg.windowMaxEvents) out.push("window over the event cap");
  const bytes = w.events.reduce((n, e) => n + bytesOf(e), 0);
  if (bytes !== w.bytes) out.push(`window bytes ${w.bytes} != ${bytes}`);
  if (bytes > s.cfg.windowMaxBytes) out.push("window over the byte cap");
  if (w.lo < 1) out.push("window lo < 1");
  return out;
}
