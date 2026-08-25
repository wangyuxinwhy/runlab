import assert from "node:assert/strict";
import test from "node:test";

import { mergeConfig } from "../src/merge.js";

test("recursively merges plain objects", () => {
  const base = { server: { host: "127.0.0.1", port: 80 }, mode: "dev" };
  const overlay = { server: { port: 443 } };
  assert.deepEqual(mergeConfig(base, overlay), {
    server: { host: "127.0.0.1", port: 443 },
    mode: "dev",
  });
});

test("replaces arrays and preserves values for undefined overlays", () => {
  assert.deepEqual(
    mergeConfig(
      { plugins: ["a"], log: { level: "info", color: true } },
      { plugins: ["b"], log: { level: undefined } },
    ),
    { plugins: ["b"], log: { level: "info", color: true } },
  );
});

test("does not mutate its inputs", () => {
  const base = { nested: { a: 1 } };
  const overlay = { nested: { b: 2 } };
  mergeConfig(base, overlay);
  assert.deepEqual(base, { nested: { a: 1 } });
  assert.deepEqual(overlay, { nested: { b: 2 } });
});
