export class TtlCache {
  constructor(capacity, now = Date.now) {
    if (!Number.isInteger(capacity) || capacity <= 0) {
      throw new TypeError("capacity must be a positive integer");
    }
    this.capacity = capacity;
    this.now = now;
    this.entries = new Map();
  }

  set(key, value, ttlMs) {
    this.entries.set(key, {
      value,
      expiresAt: this.now() + ttlMs,
    });

    if (this.entries.size > this.capacity) {
      const oldest = this.entries.keys().next().value;
      this.entries.delete(oldest);
    }
  }

  get(key) {
    return this.entries.get(key)?.value;
  }
}
