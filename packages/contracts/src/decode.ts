// The PWA's event decoder: `TypedEvent::decode` (crates/core/src/event.rs) over the
// standalone validators, against the exported `SCHEMA_VERSION` (docs/design/w1-pwa.md
// §2.5). The golden cases in crates/core/tests/golden/decode/ hold both readers to the
// same outcomes.

import type { EventEnvelope } from "./generated/EventEnvelope";
import type { GenericEvent } from "./generated/GenericEvent";
import type { TypedEvent } from "./generated/TypedEvent";
import { SCHEMA_VERSION } from "./generated/schemaVersion";
import { typedKinds } from "./validate/kinds.js";
import * as validators from "./validate/validators.js";

/** What one event body decodes to. */
export type Decoded =
  /** Not an envelope: the read that carried it failed (its stream keeps its last copy). */
  | { readonly outcome: "invalid" }
  /** A known kind with a valid payload. */
  | { readonly outcome: "typed"; readonly envelope: EventEnvelope; readonly event: TypedEvent }
  /** An envelope this reader cannot decode, kept whole and shown as "unknown event". */
  | { readonly outcome: "generic"; readonly event: GenericEvent };

/**
 * Decodes one event body, in `TypedEvent::decode`'s order: the envelope schema, then a
 * schema version newer than this reader's, then a kind it does not know, then the
 * `{ kind, payload }` pair against `TypedEvent`.
 */
export function decode(body: unknown): Decoded {
  if (!validators.EventEnvelope(body)) {
    return { outcome: "invalid" };
  }
  const envelope = body;
  const generic = (why: GenericEvent["why"]): Decoded => ({
    outcome: "generic",
    event: { envelope, why },
  });
  if (envelope.schema_version > SCHEMA_VERSION) {
    return generic("newer_schema");
  }
  if (!typedKinds.has(envelope.kind)) {
    return generic("unknown_kind");
  }
  const event = { kind: envelope.kind, payload: envelope.payload };
  if (!validators.TypedEvent(event)) {
    return generic("payload_invalid");
  }
  return { outcome: "typed", envelope, event };
}
