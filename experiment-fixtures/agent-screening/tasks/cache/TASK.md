# Repair the TTL/LRU cache

Fix `TtlCache` in `src/cache.js` so the package test suite passes.

The cache has a fixed positive capacity and receives an injectable `now()` clock. `set(key, value, ttlMs)` stores a value until `now() >= expiresAt`. `get(key)` returns `undefined` for missing or expired entries and removes expired entries. A successful `get` marks the key most recently used without extending its TTL. Updating an existing key replaces its value, refreshes its TTL, and marks it most recently used.

Before capacity eviction, remove expired entries. If live entries still exceed capacity, evict the least recently used key. Reject non-positive capacities and negative or non-finite TTL values with `TypeError`.

Do not change the tests or package scripts. Run `npm test` before finishing.
