// W1-8 `decode.golden`: the decoder meets Rust's golden cases. crates/core/tests/golden.rs
// owns them (`decode_cases_match_typed_event_decode`); every committed event sample must
// decode as its own kind.

import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

import { describe, expect, test } from "vitest";

import { decode } from "./decode";

const golden = join(import.meta.dirname, "../../../crates/core/tests/golden");
const files = (dir: string) =>
  readdirSync(join(golden, dir))
    .filter((f) => f.endsWith(".json"))
    .sort();
const read = (dir: string, name: string): unknown =>
  JSON.parse(readFileSync(join(golden, dir, name), "utf8"));

type Case = {
  expect: { why: string } | { typed: string };
  envelope: Record<string, unknown>;
};

describe("decode.golden", () => {
  const cases = files("decode");

  test("covers every outcome", () => {
    expect(cases).toEqual(
      expect.arrayContaining([
        "newer_schema_known_kind.json",
        "newer_schema_unknown_kind.json",
        "payload_field_wrong_type.json",
        "payload_not_an_object.json",
        "unknown_fields_tolerated.json",
        "unknown_kind.json",
      ]),
    );
  });

  test.each(cases)("decode/%s", (name) => {
    const c = read("decode", name) as Case;
    const got = decode(c.envelope);
    if ("why" in c.expect) {
      expect(got).toEqual({
        outcome: "generic",
        event: { envelope: c.envelope, why: c.expect.why },
      });
    } else {
      expect(got.outcome).toBe("typed");
      expect(got.outcome === "typed" && got.event.kind).toBe(c.expect.typed);
    }
  });

  test.each(files("events"))("events/%s decodes as its kind", (name) => {
    const envelope = read("events", name) as { kind: string; payload: unknown };
    const got = decode(envelope);
    expect(got).toEqual({
      outcome: "typed",
      envelope,
      event: { kind: envelope.kind, payload: envelope.payload },
    });
  });

  test("a body that is not an envelope is invalid", () => {
    const envelope = read("events", "user_message.json") as Record<string, unknown>;
    for (const body of [
      null,
      "x",
      [],
      { ...envelope, seq: -1 },
      { ...envelope, seq: Number.MAX_SAFE_INTEGER + 1 },
      { ...envelope, event_id: "not-a-uuid" },
      { ...envelope, kind: undefined },
    ]) {
      expect(decode(body)).toEqual({ outcome: "invalid" });
    }
  });
});
