// What the timeline shows for one event: plain text only, blobs as their preview, and
// "unknown event" for anything the decoder keeps generic (docs/design/w1-pwa.md §2.5).

import { decode } from "@chaldea/contracts";
import type { Content } from "@chaldea/contracts/generated/Content";
import type { EventEnvelope } from "@chaldea/contracts/generated/EventEnvelope";

export interface EventText {
  readonly title: string;
  readonly body: string | null;
  /** The body is a blob's preview, not the whole content. */
  readonly preview: boolean;
}

function content(c: Content): { text: string; preview: boolean } {
  if ("blob" in c) {
    return { text: c.preview, preview: true };
  }
  if (c.encoding === "base64") {
    return { text: `(binary content, ${Math.floor((c.inline.length * 3) / 4)} bytes)`, preview: false };
  }
  return { text: c.inline, preview: false };
}

const plain = (title: string, body: string | null = null): EventText => ({
  title,
  body,
  preview: false,
});

const withContent = (title: string, c: Content): EventText => {
  const { text, preview } = content(c);
  return { title, body: text, preview };
};

const label = (kind: string) => kind.replaceAll("_", " ");

export function eventText(e: EventEnvelope): EventText {
  const d = decode(e);
  if (d.outcome !== "typed") {
    return plain("unknown event", e.kind);
  }
  const ev = d.event;
  switch (ev.kind) {
    case "user_message":
      return withContent("user", ev.payload.prompt);
    case "assistant_message":
      return withContent("assistant", ev.payload.text);
    case "tool_activity": {
      const p = ev.payload;
      return withContent(`tool ${p.tool_name} (${p.phase})`, p.output ?? p.input);
    }
    case "status_changed": {
      const p = ev.payload;
      if (p.field === "attention") {
        const names = (xs: readonly { kind: string }[]) =>
          xs.length === 0 ? "none" : xs.map((x) => label(x.kind)).join(", ");
        return plain("attention", `${names(p.from)} → ${names(p.to)}`);
      }
      return plain(label(p.field), `${label(p.from)} → ${label(p.to)}`);
    }
    case "thread_registered":
      return plain("thread registered", `${ev.payload.agent} in ${ev.payload.cwd}`);
    case "thread_closed":
      return plain("thread closed", `by ${ev.payload.by}`);
    case "input_created":
      return withContent(`input (${label(ev.payload.origin)})`, ev.payload.text);
    case "input_state_changed":
      return plain("input", `${label(ev.payload.from)} → ${label(ev.payload.to)}`);
    case "mailbox_message":
      return withContent("mailbox message", ev.payload.body);
    case "approval_requested":
      return withContent(`approval requested: ${ev.payload.tool_name}`, ev.payload.tool_input);
    case "raw_unknown":
      return withContent(`raw input (${label(ev.payload.reason)})`, ev.payload.raw);
    case "launch_exited": {
      const p = ev.payload;
      const how = p.code !== null ? `code ${p.code}` : p.signal !== null ? `signal ${p.signal}` : "";
      return plain("launch exited", `${label(p.reason)}${how ? `, ${how}` : ""}`);
    }
    case "task_set":
      return plain(`task ${label(ev.payload.to)}`, ev.payload.summary);
    case "human_write":
      return plain("human write", label(ev.payload.origin));
    default:
      return plain(label(ev.kind));
  }
}
