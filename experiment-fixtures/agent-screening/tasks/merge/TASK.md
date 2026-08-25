# Repair the configuration merge

Fix `mergeConfig` in `src/merge.js` so the package test suite passes.

Merge own enumerable properties without mutating either input. Recursively merge plain objects. Arrays and non-plain objects are atomic values and must be replaced rather than merged. `null` is an explicit replacement, while an `undefined` overlay value preserves the base value. Keys that are absent from the base must still be copied.

At every nesting level, ignore the unsafe keys `__proto__`, `prototype`, and `constructor`. The returned object and the global object prototype must not be polluted.

Do not change the tests or package scripts. Run `npm test` before finishing.
