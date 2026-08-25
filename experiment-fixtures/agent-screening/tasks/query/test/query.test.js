import assert from "node:assert/strict";
import test from "node:test";

import { parseQuery } from "../src/query.js";

test("accepts a leading question mark and decodes spaces", () => {
  assert.deepEqual(parseQuery("?full+name=Ada+Lovelace&lang=node"), {
    "full name": "Ada Lovelace",
    lang: "node",
  });
});

test("preserves repeated keys", () => {
  assert.deepEqual(parseQuery("tag=rust&tag=oci&tag=agent"), {
    tag: ["rust", "oci", "agent"],
  });
});

test("keeps equals characters after the first delimiter", () => {
  assert.deepEqual(parseQuery("token=a=b=c"), { token: "a=b=c" });
});

test("ignores empty segments", () => {
  assert.deepEqual(parseQuery("&&a=1&&"), { a: "1" });
});
