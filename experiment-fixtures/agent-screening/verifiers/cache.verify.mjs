import assert from "node:assert/strict";

const { TtlCache } = await import("file:///experiments/cache/src/cache.js");

assert.throws(() => new TtlCache(0), TypeError);
const cache = new TtlCache(2, (() => {
  let value = 100;
  const now = () => value;
  now.set = (next) => { value = next; };
  return now;
})());
const now = cache.now;
assert.throws(() => cache.set("bad", 1, -1), TypeError);
assert.throws(() => cache.set("bad", 1, Number.POSITIVE_INFINITY), TypeError);
cache.set("a", 1, 10);
cache.set("b", 2, 100);
now.set(105);
assert.equal(cache.get("a"), 1);
cache.set("a", 3, 20);
now.set(111);
assert.equal(cache.get("a"), 3);
cache.set("c", 4, 100);
assert.equal(cache.get("b"), undefined);
now.set(125);
assert.equal(cache.get("a"), undefined);
assert.equal(cache.get("c"), 4);

console.log(JSON.stringify({ verifier: "cache", valid: true }));
