import assert from "node:assert/strict";

const { mergeConfig } = await import("file:///experiments/merge/src/merge.js");

const date = new Date("2026-01-01T00:00:00Z");
const base = {
  service: { host: "localhost", tls: { enabled: false, ca: "base" } },
  values: [1, 2],
  created: date,
};
const overlay = {
  service: { tls: { enabled: true }, retries: 3 },
  values: [9],
  created: undefined,
  nullable: null,
};
const merged = mergeConfig(base, overlay);
assert.deepEqual(merged, {
  service: {
    host: "localhost",
    tls: { enabled: true, ca: "base" },
    retries: 3,
  },
  values: [9],
  created: date,
  nullable: null,
});
assert.notEqual(merged.service, base.service);
assert.notEqual(merged.service.tls, base.service.tls);

const malicious = JSON.parse('{"safe":{"__proto__":{"polluted":true}},"constructor":{"prototype":{"polluted":true}},"prototype":{"polluted":true}}');
const protectedResult = mergeConfig({ safe: { value: 1 } }, malicious);
assert.deepEqual(protectedResult, { safe: { value: 1 } });
assert.equal({}.polluted, undefined);

console.log(JSON.stringify({ verifier: "merge", valid: true }));
