import assert from "node:assert/strict";
import test from "node:test";

import { TtlCache } from "../src/cache.js";

test("expires values at the TTL boundary", () => {
  let time = 10;
  const cache = new TtlCache(2, () => time);
  cache.set("a", 1, 5);
  time = 15;
  assert.equal(cache.get("a"), undefined);
});

test("get promotes a live entry for LRU eviction", () => {
  let time = 0;
  const cache = new TtlCache(2, () => time);
  cache.set("a", 1, 100);
  cache.set("b", 2, 100);
  assert.equal(cache.get("a"), 1);
  cache.set("c", 3, 100);
  assert.equal(cache.get("b"), undefined);
  assert.equal(cache.get("a"), 1);
  assert.equal(cache.get("c"), 3);
});

test("expired entries are removed before live entries", () => {
  let time = 0;
  const cache = new TtlCache(2, () => time);
  cache.set("expired", 1, 1);
  cache.set("live", 2, 100);
  time = 2;
  cache.set("new", 3, 100);
  assert.equal(cache.get("live"), 2);
  assert.equal(cache.get("new"), 3);
});
